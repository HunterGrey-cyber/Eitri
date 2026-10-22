//! The plugin host against a fake page, editor and layout, with the demo plugin's real (fake) CLI as the
//! task. Time is passed in where it decides anything; every real wait is a 60 s hang guard.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::plugin::doc_route::DocSlot;
use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_FOLLOW_LUA, PLUGIN_OPEN_LUA};
use eitri_core::plugin::follow_feed::ViewLine;
use eitri_core::plugin::host::{LayoutPort, PagePort, PluginHost, CHECK_DEADLINE, EDITOR_DEADLINE, EDITOR_LATE_POLL};
use eitri_core::plugin::load::load_plugin;
use eitri_core::plugin::protocol::{FocusTarget, ViewCause};
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::test_support::{demo_plugin, Scratch};
use eitri_core::plugin::theme::PageTheme;
use serde_json::{json, Value};

const HANG_GUARD: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Page(RefCell<Vec<Value>>, RefCell<Vec<PageTheme>>);

impl PagePort for Page {
    fn send(&self, message: &str) {
        self.0
            .borrow_mut()
            .push(serde_json::from_str(message).expect("the host sends JSON"));
    }
    fn set_theme(&self, theme: &PageTheme) {
        self.1.borrow_mut().push(theme.clone());
    }
}

impl Page {
    fn replies_to(&self, id: u64) -> Vec<Value> {
        self.0.borrow().iter().filter(|m| m["re"] == id).cloned().collect()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.0.borrow().iter().filter(|m| m["event"] == name).cloned().collect()
    }
}

#[derive(Default)]
struct Editor {
    calls: RefCell<Vec<(&'static str, Vec<rmpv::Value>)>>,
    answers: RefCell<Vec<Answer>>,
    /// Raised to stand for an editor that restarted: `target()` is this plus one.
    session: Cell<u64>,
}

impl EditorRpc for Editor {
    fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.calls.borrow_mut().push((code, args));
        self.answers.borrow_mut().push(answer);
        pending
    }
    fn target(&self) -> Option<u64> {
        Some(self.session.get() + 1)
    }
}

#[derive(Default)]
struct Layout(RefCell<Vec<FocusTarget>>);

impl LayoutPort for Layout {
    fn focus(&self, target: FocusTarget) -> Result<(), String> {
        self.0.borrow_mut().push(target);
        Ok(())
    }
}

struct Rig {
    _s: Scratch,
    work: std::path::PathBuf,
    host: PluginHost,
    page: Rc<Page>,
    editor: Rc<Editor>,
    layout: Rc<Layout>,
    next: RefCell<u64>,
}

impl Rig {
    fn new(label: &str, edit_manifest: impl FnOnce(String) -> String) -> Rig {
        let s = Scratch::new(label);
        let (dir, settings) = demo_plugin(&s.0);
        let manifest = std::fs::read_to_string(dir.join("eitri-plugin.toml")).unwrap();
        std::fs::write(dir.join("eitri-plugin.toml"), edit_manifest(manifest)).unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let page = Rc::new(Page::default());
        let store = PluginStore::new(s.0.join("state"), "demo");
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), Some(store), Vec::new());
        let editor = Rc::new(Editor::default());
        let layout = Rc::new(Layout::default());
        host.set_editor(editor.clone());
        host.set_layout(layout.clone());
        let work = s.0.join("work");
        std::fs::create_dir_all(work.join("ex/src")).unwrap();
        std::fs::write(work.join("ex/src/lib.rs"), "fn f() {}\n").unwrap();
        Rig {
            _s: s,
            work,
            host,
            page,
            editor,
            layout,
            next: RefCell::new(1),
        }
    }

    /// Sends a request; returns its id.
    fn send(&self, verb: &str, args: Value) -> u64 {
        let id = *self.next.borrow();
        *self.next.borrow_mut() += 1;
        let text = json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string();
        self.host.page_message(&text, Instant::now());
        id
    }

    /// Pumps until `done` holds for the page, with a far clock so batches are taken at once.
    fn pump_until(&self, what: &str, done: impl Fn(&Page) -> bool) {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            self.host.pump(Instant::now() + Duration::from_secs(1));
            if done(&self.page) {
                return;
            }
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pumps at `now` until the editor has been asked `n` times: a page-supplied path is checked on a worker
    /// before the editor is asked.
    fn asked(&self, n: usize, now: Instant) {
        let deadline = Instant::now() + HANG_GUARD;
        while self.editor.calls.borrow().len() < n {
            assert!(Instant::now() < deadline, "the editor was never asked {n} time(s)");
            self.host.pump(now);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn reply(&self, id: u64) -> Value {
        self.pump_until(&format!("a reply to {id}"), |p| !p.replies_to(id).is_empty());
        self.page.replies_to(id).remove(0)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let waiter = self.host.shutdown(Instant::now());
        let deadline = Instant::now() + HANG_GUARD;
        while !waiter.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn vars() -> Value {
    json!({"course": "rust", "exercise": "ch04-02-fix-total-len"})
}

#[test]
fn hello_lists_what_is_declared_and_undeclared_verbs_are_refused() {
    let rig = Rig::new("host-caps", |m| {
        let m = m
            .replacen(
                "[\"tasks\", \"editor.open\", \"editor.assist\", \"layout.focus\", \"state\", \"net\", \"editor.follow\", \"editor.commands\"]",
                "[\"tasks\", \"state\"]",
                1,
            )
            .replacen(
                "[net]\norigins = [\"https://learn.example.org\"]\nimages = \"${images}\"\n",
                "",
                1,
            );
        // The last tables ([module], [follow], [commands.*]) need the capabilities just taken away.
        m[..m.find("\n[module]\n").unwrap() + 1].to_string()
    });
    let hello = rig.reply(rig.send("host.hello", json!({})));
    assert_eq!(hello["result"]["capabilities"], json!(["tasks", "state"]));
    assert_eq!(hello["result"]["protocol"], 1);
    assert_eq!(hello["result"]["roots"]["work"], json!(rig.work.display().to_string()));
    assert_eq!(hello["result"]["runs"], json!([]));
    let open = rig.reply(rig.send("editor.open", json!({"path": "/x"})));
    assert_eq!(open["error"]["code"], "not_declared");
    let focus = rig.reply(rig.send("layout.focus", json!({"target": "editor"})));
    assert_eq!(focus["error"]["code"], "not_declared");
}

#[test]
fn a_reloaded_page_reattaches_to_a_running_watch() {
    let rig = Rig::new("host-reattach", |m| m);
    rig.reply(rig.send("host.hello", json!({})));
    let started = rig.reply(rig.send("task.start", json!({"task": "watch", "vars": vars()})));
    let run = started["result"]["run"].as_u64().unwrap();
    rig.pump_until("the first record", |p| !p.events("task.output").is_empty());
    let line = rig.page.events("task.output")[0]["data"]["lines"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(line.contains("check_started"), "{line}");

    // The page reloads; the run goes on, and the new page's hello hands it back with its tail.
    rig.host.page_committed();
    let hello = rig.reply(rig.send("host.hello", json!({})));
    assert_eq!(
        hello["result"]["runs"],
        json!([{"run": run, "task": "watch", "vars": vars(), "tail": [line]}])
    );

    // Reloaded again and not yet ready: the end of the run is not sent to a page that has not said hello.
    rig.host.page_committed();
    let before = rig.page.events("task.exit").len();
    let cancel = rig.send("task.cancel", json!({"run": run}));
    assert_eq!(rig.reply(cancel)["ok"], true);
    let deadline = Instant::now() + HANG_GUARD;
    while !rig.host.runs().is_empty() {
        assert!(Instant::now() < deadline);
        rig.host.pump(Instant::now() + Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        rig.page.events("task.exit").len(),
        before,
        "nothing reaches a page before its hello"
    );
}

#[test]
fn task_start_refuses_by_code_and_a_missing_build_is_a_run_that_failed_to_spawn() {
    let rig = Rig::new("host-start", |m| m);
    rig.reply(rig.send("host.hello", json!({})));
    let bad = rig.reply(rig.send(
        "task.start",
        json!({"task": "prepare", "vars": {"course": "--work", "exercise": "x"}}),
    ));
    assert_eq!(bad["error"]["code"], "bad_var");
    let unknown = rig.reply(rig.send("task.start", json!({"task": "deploy", "vars": {}})));
    assert_eq!(unknown["error"]["code"], "no_such_task");
    let first = rig.reply(rig.send("task.start", json!({"task": "watch", "vars": vars()})));
    assert_eq!(first["ok"], true);
    let again = rig.reply(rig.send("task.start", json!({"task": "watch", "vars": vars()})));
    assert_eq!(again["error"]["code"], "busy");
    let no_run = rig.reply(rig.send("task.cancel", json!({"run": 999})));
    assert_eq!(no_run["error"]["code"], "no_such_task");

    std::fs::remove_file(rig.host.plugin().dir.join("target/learn")).unwrap();
    let unbuilt = rig.reply(rig.send("task.start", json!({"task": "prepare", "vars": vars()})));
    let run = unbuilt["result"]["run"].as_u64().expect("a run, not a refusal");
    rig.pump_until("its end", |p| {
        p.events("task.exit").iter().any(|e| e["data"]["run"] == run)
    });
    let exit = rig
        .page
        .events("task.exit")
        .into_iter()
        .find(|e| e["data"]["run"] == run)
        .unwrap();
    assert_eq!(exit["data"]["status"], "spawn_failed");
}

#[test]
fn editor_open_checks_roots_then_opens_and_moves_the_keys_to_the_editor() {
    let rig = Rig::new("host-open", |m| m);
    let outside = rig.reply(rig.send("editor.open", json!({"path": "/etc/passwd"})));
    assert_eq!(outside["error"]["code"], "outside_roots");
    let relative = rig.reply(rig.send("editor.open", json!({"path": "ex/src/lib.rs"})));
    assert_eq!(relative["error"]["code"], "bad_request");
    let file = rig.work.join("ex/src/lib.rs");
    let id = rig.send(
        "editor.open",
        json!({"path": file.display().to_string(), "line": 1, "col": 4}),
    );
    assert!(rig.page.replies_to(id).is_empty(), "the answer comes later");
    rig.asked(1, Instant::now());
    assert!(rig.page.replies_to(id).is_empty(), "the answer comes later");
    assert!(std::ptr::eq(rig.editor.calls.borrow()[0].0, PLUGIN_OPEN_LUA));
    let answer = rig.editor.answers.borrow_mut().remove(0);
    answer.send(Ok(rmpv::Value::Map(vec![
        ("how".into(), "edit".into()),
        ("line".into(), 1.into()),
        ("col".into(), 3.into()),
    ])));
    assert_eq!(rig.reply(id)["ok"], true);
    assert_eq!(*rig.layout.0.borrow(), [FocusTarget::Editor]);
}

#[test]
fn an_editor_that_never_answers_times_out_once() {
    let rig = Rig::new("host-timeout", |m| m);
    let file = rig.work.join("ex/src/lib.rs");
    let start = Instant::now();
    let text =
        json!({"v": 1, "id": 7, "verb": "editor.open", "args": {"path": file.display().to_string()}}).to_string();
    rig.host.page_message(&text, start);
    rig.asked(1, start);
    let next = rig.host.pump(start + Duration::from_secs(1));
    assert!(next.is_some(), "a pending answer asks for a timer");
    assert!(rig.page.replies_to(7).is_empty());
    rig.host.pump(start + Duration::from_secs(6));
    let replies = rig.page.replies_to(7);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["error"]["code"], "editor_timeout");
    // The editor answers late: the page is not told twice.
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Nil));
    assert_eq!(rig.host.pump(start + Duration::from_secs(7)), None);
    assert_eq!(rig.page.replies_to(7).len(), 1);
}

#[test]
fn assist_is_reported_and_stopping_the_plugin_clears_it() {
    let rig = Rig::new("host-assist", |m| m);
    let root = rig.work.join("ex");
    let id = rig.send(
        "editor.assist",
        json!({"root": root.display().to_string(), "diagnostics": "off", "inlay_hints": "on"}),
    );
    rig.asked(1, Instant::now());
    {
        let calls = rig.editor.calls.borrow();
        assert!(std::ptr::eq(calls[0].0, PLUGIN_ASSIST_LUA));
        assert_eq!(calls[0].1[0], rmpv::Value::from("apply"));
    }
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
        ("diagnostics".into(), false.into()),
        ("inlay_hints".into(), true.into()),
        ("buffers".into(), 2.into()),
        ("partial".into(), false.into()),
    ])));
    let reply = rig.reply(id);
    assert_eq!(
        reply["result"],
        json!({"applied": {"diagnostics": "off", "inlay_hints": "on"}, "buffers": 2, "partial": false})
    );
    rig.host.stop(Instant::now());
    let calls = rig.editor.calls.borrow();
    assert_eq!(calls.len(), 2, "the stop cleared the assist");
    assert_eq!(calls[1].1[0], rmpv::Value::from("clear"));
}

#[test]
fn a_clear_never_overtakes_an_apply_that_is_still_out() {
    let rig = Rig::new("host-assist-order", |m| m);
    let root = rig.work.join("ex");
    let apply = rig.send(
        "editor.assist",
        json!({"root": root.display().to_string(), "diagnostics": "off", "inlay_hints": "on"}),
    );
    rig.asked(1, Instant::now());
    // The plugin is closed before the editor has answered the apply: the clear waits behind it.
    let waiting = rig.host.stop(Instant::now());
    assert!(waiting.is_some(), "a held clear needs the pump to run again");
    assert_eq!(rig.editor.calls.borrow().len(), 1, "only the apply has been sent");
    rig.host.pump(Instant::now());
    assert_eq!(
        rig.editor.calls.borrow().len(),
        1,
        "still waiting for the apply's answer"
    );

    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
        ("diagnostics".into(), false.into()),
        ("inlay_hints".into(), true.into()),
        ("buffers".into(), 1.into()),
        ("partial".into(), false.into()),
    ])));
    rig.host.pump(Instant::now());
    {
        let calls = rig.editor.calls.borrow();
        assert_eq!(calls.len(), 2, "the clear is sent once the apply is answered");
        assert_eq!(calls[1].1[0], rmpv::Value::from("clear"));
    }
    assert!(
        rig.page.replies_to(apply).is_empty(),
        "the replaced document is not answered"
    );
}

#[test]
fn assist_calls_go_out_one_at_a_time_in_the_order_they_came() {
    let rig = Rig::new("host-assist-queue", |m| m);
    let root = rig.work.join("ex").display().to_string();
    let first = rig.send(
        "editor.assist",
        json!({"root": root, "diagnostics": "off", "inlay_hints": "on"}),
    );
    let clear = rig.send("editor.assist_clear", json!({}));
    let second = rig.send(
        "editor.assist",
        json!({"root": root, "diagnostics": "on", "inlay_hints": "off"}),
    );
    let applied = |d: bool, i: bool| {
        rmpv::Value::Map(vec![
            ("diagnostics".into(), d.into()),
            ("inlay_hints".into(), i.into()),
            ("buffers".into(), 1.into()),
            ("partial".into(), false.into()),
        ])
    };
    let sent = |n: usize, what: &str| {
        let calls = rig.editor.calls.borrow();
        assert_eq!(calls.len(), n, "{what}");
        calls[n - 1].1[0].clone()
    };
    rig.asked(1, Instant::now());
    assert_eq!(sent(1, "only the first is out"), rmpv::Value::from("apply"));
    rig.editor.answers.borrow_mut().remove(0).send(Ok(applied(false, true)));
    rig.pump_until("the first answer", |p| !p.replies_to(first).is_empty());
    assert_eq!(sent(2, "the clear follows"), rmpv::Value::from("clear"));
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
        ("restored".into(), 1.into()),
        ("left_alone".into(), 0.into()),
    ])));
    rig.pump_until("the clear's answer", |p| !p.replies_to(clear).is_empty());
    rig.asked(3, Instant::now());
    assert_eq!(sent(3, "the second apply is last"), rmpv::Value::from("apply"));
    rig.editor.answers.borrow_mut().remove(0).send(Ok(applied(true, false)));
    rig.pump_until("the second answer", |p| !p.replies_to(second).is_empty());
}

#[test]
fn a_line_held_for_the_next_batch_reaches_a_returning_page_once() {
    let rig = Rig::new("host-reattach-once", |m| m);
    rig.reply(rig.send("host.hello", json!({})));
    let started = rig.reply(rig.send("task.start", json!({"task": "watch", "vars": vars()})));
    let run = started["result"]["run"].as_u64().unwrap();
    // The record is in the run's tail but no pump has taken it yet, when the reloaded page says hello.
    let deadline = Instant::now() + HANG_GUARD;
    while rig.host.runs().iter().all(|r| r.tail.is_empty()) {
        assert!(Instant::now() < deadline, "the watch never printed");
        std::thread::sleep(Duration::from_millis(5));
    }
    rig.host.page_committed();
    let id = rig.send("host.hello", json!({}));
    let hello = rig.page.replies_to(id).remove(0);
    let tail = hello["result"]["runs"][0]["tail"].clone();
    assert_eq!(hello["result"]["runs"][0]["run"], run);
    assert_eq!(tail.as_array().map(Vec::len), Some(1), "{hello}");
    let mark = rig.page.0.borrow().len();
    rig.host.pump(Instant::now() + Duration::from_secs(1));
    rig.host.pump(Instant::now() + Duration::from_secs(2));
    let again: Vec<Value> = rig.page.0.borrow()[mark..]
        .iter()
        .filter(|m| m["event"] == "task.output")
        .cloned()
        .collect();
    assert!(
        again.is_empty(),
        "the tail's line is not sent again as output: {again:?}"
    );
}

#[test]
fn state_and_log_verbs_are_answered_from_the_store() {
    let rig = Rig::new("host-store", |m| m);
    assert_eq!(
        rig.reply(rig.send("state.write", json!({"key": "device.token", "value": {"t": "x"}})))["ok"],
        true
    );
    assert_eq!(
        rig.reply(rig.send("state.read", json!({"key": "device.token"})))["result"],
        json!({"value": {"t": "x"}})
    );
    assert_eq!(
        rig.reply(rig.send("state.read", json!({"key": "nope"})))["result"],
        json!({"value": null})
    );
    assert_eq!(
        rig.reply(rig.send("log.append", json!({"stream": "events", "items": [{"a": 1}, {"a": 2}]})))["ok"],
        true
    );
    assert_eq!(
        rig.reply(rig.send("log.read", json!({"stream": "events"})))["result"],
        json!({"items": [{"a": 1}, {"a": 2}], "next": null})
    );
    assert_eq!(
        rig.reply(rig.send("state.read", json!({"key": "Bad/Key"})))["error"]["code"],
        "bad_request"
    );
}

#[test]
fn malformed_messages_are_answered_when_they_carry_an_id_and_dropped_when_not() {
    let rig = Rig::new("host-bad", |m| m);
    rig.host
        .page_message(r#"{"v":1,"id":3,"verb":"shell.run","args":{}}"#, Instant::now());
    assert_eq!(rig.page.replies_to(3)[0]["error"]["code"], "bad_request");
    rig.host
        .page_message(r#"{"v":1,"id":"4","verb":"host.hello","args":{}}"#, Instant::now());
    rig.host.page_message("not json", Instant::now());
    assert_eq!(rig.page.0.borrow().len(), 1, "nothing could answer the last two");
    let big = json!({"v": 1, "id": 5, "verb": "state.write", "args": {"key": "k", "value": "x".repeat(300 * 1024)}})
        .to_string();
    rig.host.page_message(&big, Instant::now());
    assert_eq!(rig.page.replies_to(5)[0]["error"]["code"], "too_large");
}

#[test]
fn an_idle_host_asks_for_no_timer() {
    let rig = Rig::new("host-idle", |m| m);
    rig.reply(rig.send("host.hello", json!({})));
    assert_eq!(rig.host.pump(Instant::now()), None);
}

#[test]
fn an_answer_still_out_when_the_page_reloads_never_reaches_the_new_page() {
    let rig = Rig::new("host-reload-answers", |m| m);
    // Explicit, equal ids: the host must not rely on the bridge's ids differing across documents.
    let raw = |id: u64, verb: &str, args: Value| {
        rig.host.page_message(
            &json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string(),
            Instant::now(),
        );
    };
    raw(1, "host.hello", json!({}));
    raw(2, "state.write", json!({"key": "k", "value": "new"}));
    rig.pump_until("the write's answer", |p| !p.replies_to(2).is_empty());

    // The old document asks the editor and the store, and reloads before either answer is taken.
    let file = rig.work.join("ex/src/lib.rs");
    raw(3, "editor.open", json!({"path": file.display().to_string()}));
    rig.asked(1, Instant::now());
    raw(4, "state.read", json!({"key": "nope"}));
    rig.host.page_committed();
    let mark = rig.page.0.borrow().len();
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
        ("how".into(), "edit".into()),
        ("line".into(), 1.into()),
        ("col".into(), 0.into()),
    ])));

    // The new document's 3 and 4 are other calls.
    raw(3, "host.hello", json!({}));
    raw(4, "state.read", json!({"key": "k"}));
    let since = |re: u64| -> Vec<Value> {
        rig.page.0.borrow()[mark..]
            .iter()
            .filter(|m| m["re"] == re)
            .cloned()
            .collect()
    };
    rig.pump_until("the new read's answer", |p| {
        p.0.borrow()[mark..]
            .iter()
            .any(|m| m["re"] == 4 && m["result"]["value"] == "new")
    });
    // The store works in order, so the old read was done first; one more pump takes any straggler.
    rig.host.pump(Instant::now() + Duration::from_secs(1));
    let threes = since(3);
    assert_eq!(threes.len(), 1, "{threes:?}");
    assert_eq!(threes[0]["result"]["host"], "eitri");
    let fours = since(4);
    assert_eq!(fours.len(), 1, "{fours:?}");
    assert_eq!(fours[0]["result"]["value"], "new");
    assert!(
        rig.layout.0.borrow().is_empty(),
        "the old document's open does not move the keys"
    );
}

#[test]
fn a_page_that_outlives_the_shutdown_cannot_start_a_run() {
    let rig = Rig::new("host-closed", |m| m);
    rig.reply(rig.send("host.hello", json!({})));
    let _waiter = rig.host.shutdown(Instant::now());
    let late = rig.send("task.start", json!({"task": "watch", "vars": vars()}));
    rig.host.pump(Instant::now() + Duration::from_secs(1));
    assert!(rig.page.replies_to(late).is_empty());
    assert!(rig.host.runs().is_empty());
}

#[test]
fn a_path_check_that_never_returns_holds_nothing_and_gives_up_at_its_deadline() {
    let rig = Rig::new("host-stalled-check", |m| m);
    // Every check waits for one message: a filesystem that does not answer until told to.
    let gate = rig.host.gate_path_checks();
    let file = rig.work.join("ex/src/lib.rs").display().to_string();
    let start = Instant::now();
    let raw = |id: u64, verb: &str, args: Value, at: Instant| {
        rig.host
            .page_message(&json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string(), at);
    };
    // The calls return although their checks cannot finish.
    raw(1, "editor.open", json!({"path": file}), start);
    raw(2, "task.start", json!({"task": "prepare", "vars": vars()}), start);
    let next = rig.host.pump(start + Duration::from_secs(1));
    assert!(next.is_some_and(|at| at <= start + Duration::from_secs(5)), "{next:?}");
    assert!(rig.page.0.borrow().is_empty());

    rig.host.pump(start + Duration::from_secs(6));
    for id in [1, 2] {
        let replies = rig.page.replies_to(id);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0]["error"]["code"], "io");
    }

    // The filesystem comes back. The given-up checks finish first (the worker keeps their order), and neither
    // opens nor starts anything; a new open goes through.
    drop(gate);
    raw(3, "editor.open", json!({"path": file}), start + Duration::from_secs(7));
    rig.asked(1, start + Duration::from_secs(7));
    assert_eq!(rig.editor.calls.borrow().len(), 1);
    assert!(rig.host.runs().is_empty(), "the given-up start started nothing");
    assert_eq!(rig.page.replies_to(1).len(), 1);
    assert_eq!(rig.page.replies_to(2).len(), 1);
}

#[test]
fn a_clear_sent_while_the_apply_root_is_checked_waits_behind_it() {
    let rig = Rig::new("host-check-order", |m| m);
    let gate = rig.host.gate_path_checks();
    let root = rig.work.join("ex").display().to_string();
    let apply = rig.send(
        "editor.assist",
        json!({"root": root, "diagnostics": "off", "inlay_hints": "on"}),
    );
    let clear = rig.send("editor.assist_clear", json!({}));
    rig.host.pump(Instant::now());
    assert!(
        rig.editor.calls.borrow().is_empty(),
        "the clear does not go ahead of the apply"
    );
    gate.send(()).unwrap();
    rig.asked(1, Instant::now());
    assert_eq!(rig.editor.calls.borrow()[0].1[0], rmpv::Value::from("apply"));
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
        ("diagnostics".into(), false.into()),
        ("inlay_hints".into(), true.into()),
        ("buffers".into(), 1.into()),
        ("partial".into(), false.into()),
    ])));
    rig.asked(2, Instant::now());
    assert_eq!(rig.editor.calls.borrow()[1].1[0], rmpv::Value::from("clear"));
    assert_eq!(rig.reply(apply)["ok"], true);
    assert!(
        rig.page.replies_to(clear).is_empty(),
        "the clear has not been answered yet"
    );
}

#[test]
fn an_apply_that_timed_out_still_holds_the_calls_behind_it_until_it_is_answered() {
    let rig = Rig::new("host-late-apply", |m| m);
    let root = rig.work.join("ex").display().to_string();
    let file = rig.work.join("ex/src/lib.rs").display().to_string();
    let start = Instant::now();
    let raw = |id: u64, verb: &str, args: Value| {
        rig.host.page_message(
            &json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string(),
            start,
        );
    };
    let assists = || {
        rig.editor
            .calls
            .borrow()
            .iter()
            .filter(|(code, _)| std::ptr::eq(*code, PLUGIN_ASSIST_LUA))
            .map(|(_, args)| args[0].clone())
            .collect::<Vec<_>>()
    };
    raw(
        1,
        "editor.assist",
        json!({"root": root, "diagnostics": "off", "inlay_hints": "on"}),
    );
    rig.asked(1, start);
    raw(2, "editor.assist_clear", json!({}));
    raw(
        3,
        "editor.assist",
        json!({"root": root, "diagnostics": "on", "inlay_hints": "off"}),
    );
    // Checks finish in order: once this open reaches the editor, the second apply's root has been checked.
    raw(4, "editor.open", json!({"path": file}));
    rig.asked(2, start);
    let answers: Vec<Answer> = rig.editor.answers.borrow_mut().drain(..).collect();
    let mut answers = answers.into_iter();
    let apply_answer = answers.next().unwrap();

    // Past every deadline: the page is told, but the apply may still reach nvim, so nothing behind it is sent.
    let next = rig.host.pump(start + Duration::from_secs(6));
    for id in [1, 2, 3] {
        assert_eq!(rig.page.replies_to(id)[0]["error"]["code"], "editor_timeout", "{id}");
    }
    assert_eq!(assists(), [rmpv::Value::from("apply")]);
    assert!(next.is_some(), "the late apply is still looked at");
    rig.host.pump(start + Duration::from_secs(7));
    assert_eq!(assists().len(), 1);

    // nvim runs the apply at last; only then does the clear go out, and the second apply waits for the clear
    // although its own deadline has long passed.
    apply_answer.send(Ok(rmpv::Value::Nil));
    rig.host.pump(start + Duration::from_secs(8));
    assert_eq!(assists(), [rmpv::Value::from("apply"), rmpv::Value::from("clear")]);
    rig.host.pump(start + Duration::from_secs(9));
    assert_eq!(assists().len(), 2, "the second apply waits for the clear");
    rig.editor.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Nil));
    rig.host.pump(start + Duration::from_secs(10));
    assert_eq!(
        assists(),
        [
            rmpv::Value::from("apply"),
            rmpv::Value::from("clear"),
            rmpv::Value::from("apply")
        ]
    );
    for id in [1, 2, 3] {
        assert_eq!(rig.page.replies_to(id).len(), 1, "told once: {id}");
    }
}

#[test]
fn hello_carries_every_bool_and_choice_setting() {
    let rig = Rig::new("host-settings", |m| m);
    let hello = rig.reply(rig.send("host.hello", json!({})));
    assert_eq!(hello["result"]["settings"], json!({"auto": false, "images": "https"}));
    let fixture: Value = serde_json::from_str(include_str!("../src/plugin/fixtures/rep-hello-settings.json")).unwrap();
    assert_eq!(
        hello["result"].as_object().unwrap().keys().collect::<Vec<_>>(),
        fixture["result"].as_object().unwrap().keys().collect::<Vec<_>>()
    );
}

// The follow host: the demo plugin declares editor.follow (markdown and rst) and two nvim commands.

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The demo plugin with the window's follow socket and scratch directory given.
fn follow_rig(label: &str) -> Rig {
    let rig = Rig::new(label, |m| m);
    let dir = rig._s.0.clone();
    rig.host
        .set_follow_feed(dir.join("f-socket"), Some(dir.join("scratch")));
    rig
}

fn view(buf: u64, tick: u64) -> ViewLine {
    ViewLine {
        owner: "demo".into(),
        seq: 0,
        buf,
        name: "/w/notes/a.md".into(),
        filetype: "markdown".into(),
        tick,
        current: true,
        line_count: 100,
        top: 1,
        top_fraction: 0.0,
        bottom: 40,
        cursor_line: 1,
        cursor_col: 0,
        cause: ViewCause::User,
        token: None,
    }
}

fn map(pairs: &[(&str, rmpv::Value)]) -> rmpv::Value {
    rmpv::Value::Map(pairs.iter().map(|(k, v)| (rmpv::Value::from(*k), v.clone())).collect())
}

fn strings(items: &[&str]) -> rmpv::Value {
    rmpv::Value::Array(items.iter().map(|s| rmpv::Value::from(*s)).collect())
}

fn opt(opts: &rmpv::Value, key: &str) -> rmpv::Value {
    opts.as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(key)))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("no {key} in {opts:?}"))
}

/// Answers the oldest call still waiting.
fn answer(rig: &Rig, value: rmpv::Value) {
    rig.editor.answers.borrow_mut().remove(0).send(Ok(value));
}

fn ok() -> rmpv::Value {
    map(&[("ok", true.into())])
}

fn lines(buf: u64, tick: u64, text: &[&str]) -> rmpv::Value {
    map(&[("buf", buf.into()), ("tick", tick.into()), ("lines", strings(text))])
}

fn calls(rig: &Rig) -> usize {
    rig.editor.calls.borrow().len()
}

/// The newest editor call: the follow snippet's op and its options.
fn last_call(rig: &Rig) -> (String, rmpv::Value) {
    let calls = rig.editor.calls.borrow();
    let (code, args) = calls.last().expect("an editor call");
    assert!(std::ptr::eq(*code, PLUGIN_FOLLOW_LUA), "the follow chunk itself");
    (args[0].as_str().unwrap().to_owned(), args[1].clone())
}

fn ops_since(rig: &Rig, from: usize) -> Vec<String> {
    rig.editor.calls.borrow()[from..]
        .iter()
        .map(|(_, args)| args[0].as_str().unwrap().to_owned())
        .collect()
}

/// Installs the snippet: one pump sends the install, a second takes its answer.
fn install(rig: &Rig, t: Instant) {
    rig.host.pump(t);
    assert_eq!(last_call(rig).0, "install");
    answer(rig, map(&[("ok", true.into()), ("skipped", strings(&[]))]));
    rig.host.pump(t);
}

/// Said hello, called editor.follow, on screen, installed and following.
fn following(label: &str) -> (Rig, Instant) {
    let rig = follow_rig(label);
    let t0 = Instant::now();
    rig.send("host.hello", json!({}));
    let id = rig.send("editor.follow", json!({}));
    assert_eq!(rig.page.replies_to(id)[0]["ok"], true);
    rig.host.set_on_screen(true, t0);
    install(&rig, t0);
    let (op, opts) = last_call(&rig);
    assert_eq!((op.as_str(), opt(&opts, "on")), ("follow", true.into()));
    answer(&rig, ok());
    assert_eq!(rig.host.pump(t0), None, "following and idle");
    (rig, t0)
}

#[test]
fn nothing_reaches_nvim_before_the_follow_feed_is_set() {
    let rig = Rig::new("host-follow-nofeed", |m| m);
    let t0 = Instant::now();
    rig.send("host.hello", json!({}));
    rig.send("editor.follow", json!({}));
    rig.host.editor_session(t0);
    rig.host.follow_view(view(3, 41), t0);
    rig.host.set_on_screen(true, t0);
    assert_eq!(rig.host.pump(t0), None);
    assert_eq!(calls(&rig), 0, "no editor call at all");
    let dir = rig._s.0.clone();
    rig.host.set_follow_feed(dir.join("f-socket"), None);
    rig.host.pump(t0);
    assert_eq!(calls(&rig), 1);
    assert_eq!(last_call(&rig).0, "install");
}

#[test]
fn an_early_install_is_asked_again_and_at_once_at_vimenter() {
    let rig = follow_rig("host-follow-early");
    let t0 = Instant::now();
    rig.host.pump(t0);
    assert_eq!(last_call(&rig).0, "install");
    let early = || map(&[("ok", false.into()), ("early", true.into())]);
    answer(&rig, early());
    assert_eq!(rig.host.pump(t0), Some(t0 + ms(250)));
    assert!(rig.host.skipped_commands().is_empty());
    rig.host.pump(t0 + ms(100));
    assert_eq!(calls(&rig), 1, "not asked again before 250 ms");
    rig.host.pump(t0 + ms(250));
    assert_eq!(calls(&rig), 2);
    assert_eq!(last_call(&rig).0, "install");
    answer(&rig, early());
    rig.host.pump(t0 + ms(250));
    assert_eq!(calls(&rig), 2);
    rig.host.editor_session(t0 + ms(300));
    rig.host.pump(t0 + ms(300));
    assert_eq!(calls(&rig), 3, "at once once nvim has started");
    assert_eq!(last_call(&rig).0, "install");
}

#[test]
fn a_view_of_an_unlisted_filetype_or_a_scratch_buffer_is_dropped() {
    let (rig, t0) = following("host-follow-drop");
    let before = calls(&rig);
    let mut lua = view(3, 41);
    lua.filetype = "lua".into();
    rig.host.follow_view(lua, t0);
    let mut draft = view(4, 2);
    draft.name = rig._s.0.join("scratch/1-draft.md").display().to_string();
    rig.host.follow_view(draft, t0);
    assert!(rig.page.events("editor.view").is_empty());
    assert_eq!(rig.host.pump(t0 + ms(500)), None);
    assert_eq!(calls(&rig), before, "no text call for either");
}

#[test]
fn editor_follow_comes_after_hello_and_needs_the_capability() {
    let rig = follow_rig("host-follow-gate");
    let early = rig.reply(rig.send("editor.follow", json!({})));
    assert_eq!(early["error"]["code"], "bad_request");
    assert_eq!(early["error"]["message"], "editor.follow comes after host.hello");

    let undeclared = Rig::new("host-follow-undeclared", |m| {
        m.replacen("\"editor.follow\", ", "", 1).replacen(
            "[follow]\nfiletypes = [\"markdown\", \"rst\"]\nauto_show = \"auto\"\n",
            "",
            1,
        )
    });
    undeclared.send("host.hello", json!({}));
    let refused = undeclared.reply(undeclared.send("editor.follow", json!({})));
    assert_eq!(refused["error"]["code"], "not_declared");
    assert!(undeclared.host.wants_follow_feed(), "it still defines nvim commands");
}

#[test]
fn install_carries_the_commands_the_filetypes_and_the_auto_setting() {
    let rig = follow_rig("host-follow-install");
    let t0 = Instant::now();
    rig.host.editor_session(t0);
    rig.host.pump(t0);
    let (op, opts) = last_call(&rig);
    assert_eq!(op, "install");
    let dir = rig._s.0.clone();
    assert_eq!(opt(&opts, "owner"), "demo".into());
    assert_eq!(
        opt(&opts, "socket"),
        rmpv::Value::Binary(dir.join("f-socket").display().to_string().into_bytes())
    );
    assert_eq!(
        opt(&opts, "scratch_dir"),
        rmpv::Value::Binary(dir.join("scratch").display().to_string().into_bytes())
    );
    assert_eq!(opt(&opts, "filetypes"), strings(&["markdown", "rst"]));
    assert_eq!(opt(&opts, "auto"), false.into());
    assert_eq!(
        opt(&opts, "commands"),
        rmpv::Value::Array(vec![strings(&["DemoShow", "show"]), strings(&["DemoToggle", "toggle"])])
    );
    answer(&rig, map(&[("ok", true.into()), ("skipped", strings(&["DemoShow"]))]));
    rig.host.pump(t0);
    assert_eq!(rig.host.skipped_commands(), ["DemoShow"]);
    rig.host.editor_session(t0 + ms(10));
    rig.host.pump(t0 + Duration::from_secs(10));
    assert_eq!(calls(&rig), 1, "one install per editor session");
    assert!(rig.host.wants_follow_feed());
    assert!(!rig.host.auto_show());
}

#[test]
fn following_starts_only_when_a_following_document_is_on_screen() {
    let rig = follow_rig("host-follow-screen");
    let t0 = Instant::now();
    install(&rig, t0);
    rig.send("host.hello", json!({}));
    let id = rig.send("editor.follow", json!({}));
    assert_eq!(
        rig.page.replies_to(id)[0]["result"],
        json!({"filetypes": ["markdown", "rst"]})
    );
    assert_eq!(rig.host.pump(t0), None);
    assert_eq!(calls(&rig), 1, "no follow call while off screen");
    let follow_call = |on: bool| {
        rig.host.pump(t0);
        let (op, opts) = last_call(&rig);
        assert_eq!((op.as_str(), opt(&opts, "on")), ("follow", on.into()));
        answer(&rig, ok());
        rig.host.pump(t0);
    };
    rig.host.set_on_screen(true, t0);
    follow_call(true);
    rig.host.set_on_screen(false, t0);
    follow_call(false);
    rig.host.set_on_screen(true, t0);
    follow_call(true);
    rig.host.page_committed();
    follow_call(false);
    assert_eq!(calls(&rig), 5);
}

#[test]
fn a_view_reaches_a_following_page_on_screen_and_its_text_is_pulled() {
    let (rig, t0) = following("host-follow-view");
    rig.host.follow_view(view(3, 41), t0);
    let views = rig.page.events("editor.view");
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0]["data"],
        json!({"buf": 3, "name": "/w/notes/a.md", "filetype": "markdown", "tick": 41, "current": true,
               "line_count": 100, "top": 1, "top_fraction": 0.0, "bottom": 40, "cursor": {"line": 1, "col": 0},
               "cause": "user", "doc": null})
    );
    rig.host.pump(t0);
    let (op, opts) = last_call(&rig);
    assert_eq!(op, "text");
    assert_eq!(opt(&opts, "owner"), "demo".into());
    assert_eq!(opt(&opts, "buf"), 3.into());
    assert_eq!(opt(&opts, "max_bytes"), 4_194_304.into());
    assert_eq!(opt(&opts, "max_lines"), 50_000.into());
    answer(&rig, lines(3, 41, &["# a", "b"]));
    rig.host.pump(t0);
    assert_eq!(
        rig.page.events("editor.text"),
        [json!({"v": 1, "event": "editor.text",
                "data": {"buf": 3, "tick": 41, "name": "/w/notes/a.md", "text": "# a\nb"}})]
    );
}

#[test]
fn nothing_is_sent_off_screen_or_before_editor_follow() {
    let rig = follow_rig("host-follow-quiet");
    let t0 = Instant::now();
    install(&rig, t0);
    rig.send("host.hello", json!({}));
    rig.host.set_on_screen(true, t0);
    rig.host.follow_view(view(3, 41), t0);
    assert_eq!(rig.host.pump(t0), None);
    rig.host.set_on_screen(false, t0);
    rig.send("editor.follow", json!({}));
    rig.host.follow_view(view(3, 42), t0);
    assert_eq!(rig.host.pump(t0 + ms(500)), None);
    assert!(rig.page.events("editor.view").is_empty());
    assert!(rig.page.events("editor.text").is_empty());
    assert_eq!(calls(&rig), 1, "only the install");
}

#[test]
fn typing_pulls_at_most_every_200_ms_and_the_last_change_is_pulled() {
    let (rig, t0) = following("host-follow-typing");
    let from = calls(&rig);
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    answer(&rig, lines(3, 41, &["a"]));
    rig.host.pump(t0);
    rig.host.follow_view(view(3, 42), t0 + ms(50));
    assert_eq!(rig.host.pump(t0 + ms(50)), Some(t0 + ms(200)));
    rig.host.follow_view(view(3, 43), t0 + ms(120));
    rig.host.pump(t0 + ms(120));
    assert_eq!(ops_since(&rig, from), ["text"], "none in between");
    rig.host.pump(t0 + ms(200));
    assert_eq!(ops_since(&rig, from), ["text", "text"]);
    answer(&rig, lines(3, 43, &["abc"]));
    assert_eq!(rig.host.pump(t0 + ms(200)), None);
    rig.host.pump(t0 + Duration::from_secs(1));
    assert_eq!(
        ops_since(&rig, from),
        ["text", "text"],
        "the answer for 43 ends the pulls"
    );
    let ticks: Vec<Value> = rig
        .page
        .events("editor.text")
        .iter()
        .map(|e| e["data"]["tick"].clone())
        .collect();
    assert_eq!(ticks, [json!(41), json!(43)]);
}

#[test]
fn a_too_large_answer_becomes_the_notice_and_pulls_slow_down() {
    let (rig, t0) = following("host-follow-large");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    answer(
        &rig,
        map(&[
            ("buf", 3.into()),
            ("tick", 41.into()),
            ("too_large", true.into()),
            ("bytes", 5_452_595.into()),
            ("lines", 61_000.into()),
        ]),
    );
    rig.host.pump(t0);
    assert_eq!(
        rig.page.events("editor.text")[0]["data"],
        json!({"buf": 3, "tick": 41, "name": "/w/notes/a.md",
               "too_large": {"bytes": 5_452_595, "lines": 61_000, "max_bytes": 4_194_304, "max_lines": 50_000}})
    );
    rig.host.follow_view(view(3, 42), t0 + ms(300));
    assert_eq!(rig.host.pump(t0 + ms(300)), Some(t0 + Duration::from_secs(1)));
}

#[test]
fn a_gone_answer_or_a_wiped_line_closes_the_buffer() {
    let (rig, t0) = following("host-follow-gone");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    answer(&rig, map(&[("gone", true.into()), ("buf", 3.into())]));
    rig.host.pump(t0);
    assert_eq!(
        rig.page.events("editor.closed"),
        [json!({"v": 1, "event": "editor.closed", "data": {"buf": 3, "reason": "wiped"}})]
    );
    let before = calls(&rig);
    assert_eq!(rig.host.pump(t0 + Duration::from_secs(2)), None);
    assert_eq!(calls(&rig), before, "no further pull for that buffer");

    rig.host.follow_view(view(5, 1), t0 + Duration::from_secs(2));
    rig.host.pump(t0 + Duration::from_secs(2));
    answer(&rig, lines(5, 1, &["x"]));
    rig.host.pump(t0 + Duration::from_secs(2));
    rig.host.follow_gone(5, t0 + Duration::from_secs(3));
    assert_eq!(
        rig.page.events("editor.closed")[1]["data"],
        json!({"buf": 5, "reason": "wiped"})
    );
    let before = calls(&rig);
    assert_eq!(rig.host.pump(t0 + Duration::from_secs(5)), None);
    assert_eq!(calls(&rig), before);
}

#[test]
fn a_pull_answered_after_the_view_moved_to_another_buffer_is_dropped() {
    let (rig, t0) = following("host-follow-moved");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    assert_eq!(opt(&last_call(&rig).1, "buf"), 3.into());
    let mut other = view(5, 1);
    other.name = "/w/notes/b.md".into();
    rig.host.follow_view(other, t0 + ms(10));
    answer(&rig, lines(3, 41, &["# a"]));
    rig.host.pump(t0 + ms(20));
    assert!(
        rig.page.events("editor.text").is_empty(),
        "buffer 3's text is not what the page shows any more"
    );
    let (op, opts) = last_call(&rig);
    assert_eq!(
        (op.as_str(), opt(&opts, "buf")),
        ("text", 5.into()),
        "the new buffer is pulled at once"
    );
    answer(&rig, lines(5, 1, &["# b"]));
    rig.host.pump(t0 + ms(30));
    let texts = rig.page.events("editor.text");
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0]["data"]["buf"], 5);
    assert_eq!(texts[0]["data"]["name"], "/w/notes/b.md");
}

#[test]
fn a_buffer_wiped_while_its_pull_is_out_is_closed_once_and_its_text_dropped() {
    // The pull answers with the lines it read before the wipe.
    let (rig, t0) = following("host-follow-wiped-lines");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    rig.host.follow_gone(3, t0 + ms(10));
    answer(&rig, lines(3, 41, &["# a"]));
    rig.host.pump(t0 + ms(20));
    assert_eq!(rig.page.events("editor.closed").len(), 1);
    assert!(
        rig.page.events("editor.text").is_empty(),
        "nothing follows the buffer's end"
    );
    let before = calls(&rig);
    assert_eq!(rig.host.pump(t0 + Duration::from_secs(2)), None);
    assert_eq!(calls(&rig), before);

    // The pull answers that the buffer is gone.
    let (rig, t0) = following("host-follow-wiped-gone");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    rig.host.follow_gone(3, t0 + ms(10));
    answer(&rig, map(&[("gone", true.into()), ("buf", 3.into())]));
    rig.host.pump(t0 + ms(20));
    assert_eq!(
        rig.page.events("editor.closed"),
        [json!({"v": 1, "event": "editor.closed", "data": {"buf": 3, "reason": "wiped"}})],
        "the page hears of the end once"
    );
}

#[test]
fn an_editor_restart_reinstalls_follows_again_and_drops_old_answers() {
    let (rig, t0) = following("host-follow-restart");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    let from = calls(&rig);
    assert_eq!(last_call(&rig).0, "text");
    let old = rig.editor.answers.borrow_mut().remove(0);

    rig.editor.session.set(1);
    rig.host.pump(t0 + ms(10));
    assert_eq!(ops_since(&rig, from), ["install"], "the old pull is not waited for");
    answer(&rig, map(&[("ok", true.into()), ("skipped", strings(&[]))]));
    rig.host.pump(t0 + ms(20));
    assert_eq!(ops_since(&rig, from), ["install", "follow"]);
    assert_eq!(opt(&last_call(&rig).1, "on"), true.into());
    answer(&rig, ok());
    rig.host.pump(t0 + ms(30));

    old.send(Ok(lines(3, 41, &["from the old editor"])));
    rig.host.pump(t0 + ms(40));
    rig.host.pump(t0 + Duration::from_secs(1));
    assert!(
        rig.page.events("editor.text").is_empty(),
        "the old session's answer is dropped"
    );
    assert_eq!(ops_since(&rig, from), ["install", "follow"]);
}

#[test]
fn the_answer_of_a_replaced_document_is_dropped() {
    let (rig, t0) = following("host-follow-replaced");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    answer(&rig, lines(3, 41, &["one"]));
    rig.host.pump(t0);
    assert_eq!(rig.page.events("editor.text").len(), 1);
    rig.host.follow_view(view(3, 42), t0 + ms(300));
    rig.host.pump(t0 + ms(300));
    assert_eq!(last_call(&rig).0, "text");

    rig.host.page_committed();
    answer(&rig, lines(3, 42, &["two"]));
    rig.host.pump(t0 + ms(300));
    assert_eq!(rig.page.events("editor.text").len(), 1, "the pull's answer is dropped");

    rig.send("host.hello", json!({}));
    let mark = rig.page.0.borrow().len();
    let id = rig.send("editor.follow", json!({}));
    let sent: Vec<Value> = rig.page.0.borrow()[mark..].to_vec();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0]["re"], id, "the reply first");
    assert_eq!(sent[1]["event"], "editor.view");
    assert_eq!(sent[1]["data"]["tick"], 42);
    assert_eq!(
        sent[2],
        json!({"v": 1, "event": "editor.text",
               "data": {"buf": 3, "tick": 41, "name": "/w/notes/a.md", "text": "one"}})
    );
}

#[test]
fn editor_closed_tells_a_following_page_and_stop_tears_down() {
    let (rig, t0) = following("host-follow-stop");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    answer(&rig, lines(3, 41, &["a"]));
    rig.host.pump(t0);
    rig.host.editor_closed(t0);
    assert_eq!(
        rig.page.events("editor.closed"),
        [json!({"v": 1, "event": "editor.closed", "data": {"buf": 3, "reason": "editor"}})]
    );
    assert!(rig.host.stop(t0).is_some(), "the teardown needs a pump");
    rig.host.pump(t0);
    let (op, opts) = last_call(&rig);
    assert_eq!(op, "teardown");
    assert_eq!(opt(&opts, "owner"), "demo".into());
    answer(&rig, ok());
    assert_eq!(rig.host.pump(t0), None);

    rig.send("host.hello", json!({}));
    let refused = rig.reply(rig.send("editor.follow", json!({})));
    assert_eq!(refused["error"]["code"], "editor_unavailable");
    assert_eq!(
        refused["error"]["message"],
        "stopped with prefix x; the plugin comes back when the window restarts"
    );
}

#[test]
fn a_late_follow_call_is_kept_and_the_page_is_not_answered_twice() {
    let (rig, t0) = following("host-follow-late");
    rig.host.follow_view(view(3, 41), t0);
    rig.host.pump(t0);
    let from = calls(&rig);
    let late = t0 + EDITOR_DEADLINE + Duration::from_secs(1);
    assert_eq!(rig.host.pump(late), Some(late + EDITOR_LATE_POLL));
    rig.host.follow_view(view(3, 42), late);
    rig.host.set_on_screen(false, late);
    rig.host.pump(late + Duration::from_secs(1));
    assert_eq!(calls(&rig), from, "nothing else reaches nvim before the answer");
    answer(&rig, lines(3, 41, &["a"]));
    rig.host.pump(late + Duration::from_secs(2));
    assert_eq!(ops_since(&rig, from), ["follow"]);
    assert_eq!(opt(&last_call(&rig).1, "on"), false.into());
    assert!(
        rig.page.events("editor.text").is_empty(),
        "a hidden page is not sent the text"
    );
}

#[test]
fn idle_with_the_preview_hidden_asks_for_no_timer() {
    let rig = follow_rig("host-follow-idle");
    let t0 = Instant::now();
    rig.host.pump(t0);
    answer(&rig, map(&[("ok", true.into()), ("skipped", strings(&[]))]));
    assert_eq!(rig.host.pump(t0), None);
    rig.send("host.hello", json!({}));
    rig.send("editor.follow", json!({}));
    rig.host.follow_view(view(3, 41), t0);
    assert_eq!(rig.host.pump(t0 + ms(10)), None);
    assert_eq!(calls(&rig), 1);
}

// The follow verbs and the document route's project: the followed document is <s>/proj/docs/a.md in a
// repository at <s>/proj, which is also the window's root.

struct Doc {
    rig: Rig,
    t0: Instant,
    s: PathBuf,
    proj: PathBuf,
    a: PathBuf,
    slot: Rc<RefCell<DocSlot>>,
}

fn touch(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Following, with the route's slot and the window root given and the project on disk; no view yet and no path
/// check asked, so a test may still gate the checks.
fn doc_base(label: &str) -> Doc {
    let (rig, t0) = following(label);
    let s = rig._s.0.clone();
    let proj = s.join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    touch(&proj.join("README.md"), "# readme\n");
    touch(&proj.join("src/main.rs"), "fn main() {}\n");
    touch(&proj.join("docs/a.md"), "# a\n");
    touch(&proj.join("docs/other.md"), "# other\n");
    touch(&s.join("outside.md"), "# outside\n");
    std::os::unix::fs::symlink(s.join("outside.md"), proj.join("docs/out.md")).unwrap();
    let slot = Rc::new(RefCell::new(DocSlot::default()));
    rig.host.set_doc_slot(slot.clone());
    rig.host.set_window_root(proj.clone());
    let a = proj.join("docs/a.md");
    Doc {
        rig,
        t0,
        s,
        proj,
        a,
        slot,
    }
}

fn named_view(name: &Path, buf: u64, tick: u64) -> ViewLine {
    let mut v = view(buf, tick);
    v.name = name.display().to_string();
    v
}

/// Pumps until the route's slot serves `dir`.
fn wait_slot(d: &Doc, dir: &Path) {
    let deadline = Instant::now() + HANG_GUARD;
    while d.slot.borrow().dir.as_deref() != Some(dir) {
        assert!(Instant::now() < deadline, "the slot never served {}", dir.display());
        d.rig.host.pump(d.t0);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The document shown: its view, its text pulled and answered, its project known.
fn shown(label: &str) -> Doc {
    let d = doc_base(label);
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    d.rig.host.pump(d.t0);
    assert_eq!(last_call(&d.rig).0, "text");
    answer(&d.rig, lines(3, 41, &["# a"]));
    d.rig.host.pump(d.t0);
    wait_slot(&d, &d.proj);
    d
}

fn open_calls(rig: &Rig) -> Vec<rmpv::Value> {
    rig.editor
        .calls
        .borrow()
        .iter()
        .filter(|(code, _)| std::ptr::eq(*code, PLUGIN_OPEN_LUA))
        .map(|(_, args)| args[0].clone())
        .collect()
}

fn has_key(opts: &rmpv::Value, key: &str) -> bool {
    opts.as_map()
        .is_some_and(|m| m.iter().any(|(k, _)| k.as_str() == Some(key)))
}

fn binary(path: &Path) -> rmpv::Value {
    rmpv::Value::Binary(path.display().to_string().into_bytes())
}

fn last_view(rig: &Rig) -> Value {
    rig.page.events("editor.view").last().expect("a view")["data"].clone()
}

fn link(d: &Doc, target: &str) -> u64 {
    d.rig.send("editor.open_link", json!({"buf": 3, "target": target}))
}

/// Sends a link that passes its check; returns the open call's options once it is asked.
fn link_opened(d: &Doc, target: &str) -> (u64, rmpv::Value) {
    let before = open_calls(&d.rig).len();
    let id = link(d, target);
    let deadline = Instant::now() + HANG_GUARD;
    while open_calls(&d.rig).len() == before {
        assert!(
            Instant::now() < deadline,
            "{target} was never opened: {:?}",
            d.rig.page.replies_to(id)
        );
        d.rig.host.pump(d.t0);
        std::thread::sleep(Duration::from_millis(2));
    }
    (id, open_calls(&d.rig).pop().unwrap())
}

#[test]
fn reveal_answers_applied_and_only_the_newest_parked_reveal_is_sent() {
    let d = shown("host-reveal-newest");
    let t = d.t0 + ms(300);
    d.rig.host.follow_view(named_view(&d.a, 3, 42), t);
    d.rig.host.pump(t);
    assert_eq!(last_call(&d.rig).0, "text", "a pull is out, so reveals wait");
    let from = calls(&d.rig);
    let reveal = |line: u32, token: u64| {
        d.rig
            .send("editor.reveal", json!({"buf": 3, "line": line, "token": token}))
    };
    let a = reveal(10, 1);
    let b = reveal(20, 2);
    let c = reveal(30, 3);
    assert_eq!(d.rig.page.replies_to(a)[0]["result"], json!({"applied": false}));
    assert_eq!(d.rig.page.replies_to(b)[0]["result"], json!({"applied": false}));
    assert!(d.rig.page.replies_to(c).is_empty());
    answer(&d.rig, lines(3, 42, &["# a", "b"]));
    d.rig.host.pump(t);
    assert_eq!(ops_since(&d.rig, from), ["reveal"]);
    let (_, opts) = last_call(&d.rig);
    assert_eq!(opt(&opts, "owner"), "demo".into());
    assert_eq!(opt(&opts, "buf"), 3.into());
    assert_eq!(opt(&opts, "line"), 30.into());
    assert_eq!(opt(&opts, "token"), 3.into());
    answer(&d.rig, map(&[("applied", true.into())]));
    d.rig.host.pump(t);
    assert_eq!(d.rig.page.replies_to(c)[0]["result"], json!({"applied": true}));
    assert_eq!(d.rig.page.replies_to(a).len(), 1, "answered once");
    d.rig.host.pump(t + Duration::from_secs(1));
    assert_eq!(ops_since(&d.rig, from), ["reveal"], "nothing else");
}

#[test]
fn a_reveal_or_jump_for_a_buffer_not_followed_is_not_applied() {
    let d = shown("host-reveal-other");
    let from = calls(&d.rig);
    let r = d.rig.send("editor.reveal", json!({"buf": 9, "line": 3, "token": 1}));
    let j = d
        .rig
        .send("editor.jump", json!({"buf": 9, "line": 3, "col": 0, "focus": true}));
    assert_eq!(d.rig.page.replies_to(r)[0]["result"], json!({"applied": false}));
    assert_eq!(d.rig.page.replies_to(j)[0]["result"], json!({"applied": false}));
    d.rig.host.pump(d.t0 + Duration::from_secs(1));
    assert_eq!(calls(&d.rig), from, "no editor call");
    assert!(d.rig.layout.0.borrow().is_empty());
}

#[test]
fn bad_arguments_are_refused_by_their_checks() {
    let rig = follow_rig("host-verbs-bad");
    let early = [
        rig.send("editor.reveal", json!({"buf": 3, "line": 1, "token": 1})),
        rig.send("editor.jump", json!({"buf": 3, "line": 1, "col": 0, "focus": false})),
        rig.send("editor.open_link", json!({"buf": 3, "target": "a.md"})),
    ];
    for (id, verb) in early
        .into_iter()
        .zip(["editor.reveal", "editor.jump", "editor.open_link"])
    {
        let reply = &rig.page.replies_to(id)[0];
        assert_eq!(reply["error"]["code"], "bad_request");
        assert_eq!(reply["error"]["message"], format!("{verb} comes after host.hello"));
    }
    rig.send("host.hello", json!({}));
    let refused = |id: u64, message: &str| {
        let reply = &rig.page.replies_to(id)[0];
        assert_eq!(reply["error"]["code"], "bad_request", "{reply}");
        assert_eq!(reply["error"]["message"], message);
    };
    refused(
        rig.send("editor.reveal", json!({"buf": 3, "line": 0, "token": 1})),
        "line must be at least 1",
    );
    refused(
        rig.send("editor.jump", json!({"buf": 3, "line": 0, "col": 0, "focus": false})),
        "line must be at least 1",
    );
    refused(
        rig.send("editor.open_link", json!({"buf": 3, "target": "a\nb.md"})),
        "target holds a control character",
    );
    refused(
        rig.send("editor.open_link", json!({"buf": 3, "target": "a".repeat(4097)})),
        "target is over 4096 bytes",
    );
    assert_eq!(calls(&rig), 0);
}

#[test]
fn jump_with_focus_moves_the_keys_only_when_applied() {
    let d = shown("host-jump-focus");
    let jump = |focus: bool| {
        let id = d
            .rig
            .send("editor.jump", json!({"buf": 3, "line": 5, "col": 0, "focus": focus}));
        d.rig.host.pump(d.t0);
        let (op, opts) = last_call(&d.rig);
        assert_eq!(op, "jump");
        assert_eq!(opt(&opts, "owner"), "demo".into());
        assert_eq!(opt(&opts, "buf"), 3.into());
        assert_eq!(opt(&opts, "line"), 5.into());
        assert_eq!(opt(&opts, "col"), 0.into());
        id
    };
    let first = jump(true);
    answer(&d.rig, map(&[("applied", true.into())]));
    d.rig.host.pump(d.t0);
    assert_eq!(d.rig.page.replies_to(first)[0]["result"], json!({"applied": true}));
    assert_eq!(*d.rig.layout.0.borrow(), [FocusTarget::Editor]);

    let second = jump(true);
    answer(&d.rig, map(&[("applied", false.into())]));
    d.rig.host.pump(d.t0);
    assert_eq!(d.rig.page.replies_to(second)[0]["result"], json!({"applied": false}));
    assert_eq!(d.rig.layout.0.borrow().len(), 1, "not applied: the keys stay");

    let third = jump(false);
    answer(&d.rig, map(&[("applied", true.into())]));
    d.rig.host.pump(d.t0);
    assert_eq!(d.rig.page.replies_to(third)[0]["result"], json!({"applied": true}));
    assert_eq!(d.rig.layout.0.borrow().len(), 1, "no focus asked: the keys stay");

    // Answered after the page was told the editor was busy: the keys stay where the user has gone since.
    let late = jump(true);
    let past = d.t0 + EDITOR_DEADLINE + Duration::from_secs(1);
    d.rig.host.pump(past);
    assert_eq!(d.rig.page.replies_to(late)[0]["error"]["code"], "editor_timeout");
    answer(&d.rig, map(&[("applied", true.into())]));
    d.rig.host.pump(past + EDITOR_LATE_POLL);
    assert_eq!(d.rig.page.replies_to(late).len(), 1, "answered once");
    assert_eq!(d.rig.layout.0.borrow().len(), 1);

    // Answered after the deadline, but before a pump told the page it was busy: the answer is passed on, and
    // the keys still stay.
    let slow = jump(true);
    answer(&d.rig, map(&[("applied", true.into())]));
    d.rig.host.pump(d.t0 + EDITOR_DEADLINE + Duration::from_secs(1));
    assert_eq!(d.rig.page.replies_to(slow)[0]["result"], json!({"applied": true}));
    assert_eq!(d.rig.layout.0.borrow().len(), 1, "a late answer moves no keys");
}

#[test]
fn open_link_resolves_against_the_document_and_stays_in_the_project() {
    let d = shown("host-open-link");
    let opened = |target: &str, path: PathBuf, line: Option<u32>| {
        let (id, opts) = link_opened(&d, target);
        assert_eq!(opt(&opts, "path"), binary(&path), "{target}");
        match line {
            Some(line) => assert_eq!(opt(&opts, "line"), line.into(), "{target}"),
            None => assert!(!has_key(&opts, "line"), "{target}: {opts:?}"),
        }
        assert!(!has_key(&opts, "col"), "{target}");
        answer(
            &d.rig,
            map(&[
                ("how", "edit".into()),
                ("line", line.unwrap_or(1).into()),
                ("col", 0.into()),
            ]),
        );
        d.rig.host.pump(d.t0);
        assert_eq!(
            d.rig.page.replies_to(id)[0]["result"],
            json!({"how": "edit", "line": line.unwrap_or(1)}),
            "{target}"
        );
    };
    opened("other.md", d.proj.join("docs/other.md"), None);
    opened("../README.md#L10", d.proj.join("README.md"), Some(10));
    opened("/src/main.rs#L3-L9", d.proj.join("src/main.rs"), Some(3));
    opened("%2E%2E/README.md#intro", d.proj.join("README.md"), None);
    opened("other.md#L0", d.proj.join("docs/other.md"), None);
    assert!(d.rig.layout.0.borrow().is_empty(), "the keys stay with the page");

    let code = |target: &str| {
        let reply = d.rig.reply(link(&d, target));
        reply["error"]["code"].as_str().unwrap_or("none").to_owned()
    };
    assert_eq!(code("../../outside.md"), "outside_roots");
    assert_eq!(code("out.md"), "outside_roots", "a symlink out of the project");
    assert_eq!(code("missing.md"), "io");
    assert_eq!(code("../docs"), "io", "a directory");
    assert_eq!(code("#intro"), "bad_request", "nothing to open");
    assert_eq!(code("bad%zz.md"), "bad_request");
    assert_eq!(code("/"), "bad_request");
    assert_eq!(
        code("//etc/hostname"),
        "io",
        "never an absolute path: <proj>/etc/hostname is missing"
    );
    let wrong = d
        .rig
        .reply(d.rig.send("editor.open_link", json!({"buf": 9, "target": "other.md"})));
    assert_eq!(wrong["error"]["code"], "bad_request");
    assert_eq!(wrong["error"]["message"], "buf is not the followed buffer");
    assert_eq!(open_calls(&d.rig).len(), 5, "no refused link reached the editor");
}

#[test]
fn open_link_checks_wait_on_a_stalled_filesystem() {
    let d = doc_base("host-open-link-stalled");
    let gate = d.rig.host.gate_path_checks();
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    d.rig.host.pump(d.t0);
    answer(&d.rig, lines(3, 41, &["# a"]));
    d.rig.host.pump(d.t0);
    let from = calls(&d.rig);
    let id = link(&d, "other.md");
    d.rig.host.pump(d.t0);
    std::thread::sleep(Duration::from_millis(50));
    d.rig.host.pump(d.t0);
    assert!(d.rig.page.replies_to(id).is_empty());
    assert_eq!(calls(&d.rig), from, "no editor call");
    d.rig.host.pump(Instant::now() + CHECK_DEADLINE);
    let reply = &d.rig.page.replies_to(id)[0];
    assert_eq!(reply["error"]["code"], "io");
    assert_eq!(reply["error"]["message"], "the filesystem did not answer within 5 s");
    drop(gate);
    wait_slot(&d, &d.proj);
    d.rig.host.pump(d.t0);
    std::thread::sleep(Duration::from_millis(50));
    d.rig.host.pump(d.t0);
    assert_eq!(calls(&d.rig), from, "nothing opened later");
    assert_eq!(d.rig.page.replies_to(id).len(), 1);
}

#[test]
fn a_link_check_that_finishes_after_its_deadline_opens_nothing() {
    let d = doc_base("host-open-link-late");
    let gate = d.rig.host.gate_path_checks();
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    d.rig.host.pump(d.t0);
    answer(&d.rig, lines(3, 41, &["# a"]));
    d.rig.host.pump(d.t0);
    let from = calls(&d.rig);
    let id = link(&d, "other.md");
    // The check finishes, but no pump takes its result before the deadline has passed.
    drop(gate);
    std::thread::sleep(Duration::from_millis(200));
    d.rig
        .host
        .pump(Instant::now() + CHECK_DEADLINE + Duration::from_secs(1));
    let reply = &d.rig.page.replies_to(id)[0];
    assert_eq!(reply["error"]["code"], "io");
    assert_eq!(reply["error"]["message"], "the filesystem did not answer within 5 s");
    d.rig
        .host
        .pump(Instant::now() + CHECK_DEADLINE + Duration::from_secs(2));
    assert_eq!(calls(&d.rig), from, "nothing opened");
    assert_eq!(d.rig.page.replies_to(id).len(), 1);
}

#[test]
fn an_editor_restart_answers_the_page_ops_waiting_in_line_and_sends_none() {
    let d = doc_base("host-verbs-restart");
    let gate = d.rig.host.gate_path_checks();
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    d.rig.host.pump(d.t0);
    assert_eq!(last_call(&d.rig).0, "text", "a pull is out, so the ops below wait");
    let reveal = d.rig.send("editor.reveal", json!({"buf": 3, "line": 10, "token": 1}));
    let jump = d
        .rig
        .send("editor.jump", json!({"buf": 3, "line": 5, "col": 0, "focus": true}));
    // Its check is held, so it is still being checked when the editor goes.
    let held = link(&d, "other.md");
    let from = calls(&d.rig);
    // The old editor's pull is never answered.
    let _old = d.rig.editor.answers.borrow_mut().remove(0);

    d.rig.editor.session.set(1);
    d.rig.host.pump(d.t0 + ms(10));
    for id in [reveal, jump] {
        let reply = &d.rig.page.replies_to(id)[0];
        assert_eq!(reply["error"]["code"], "editor_unavailable", "{reply}");
        assert_eq!(reply["error"]["message"], "the editor restarted");
    }
    assert_eq!(ops_since(&d.rig, from), ["install"]);
    answer(&d.rig, map(&[("ok", true.into()), ("skipped", strings(&[]))]));
    d.rig.host.pump(d.t0 + ms(20));
    assert_eq!(ops_since(&d.rig, from), ["install", "follow"]);
    answer(&d.rig, ok());

    drop(gate);
    let reply = d.rig.reply(held);
    assert_eq!(reply["error"]["code"], "editor_unavailable", "{reply}");
    assert_eq!(reply["error"]["message"], "the editor restarted");
    d.rig.host.pump(d.t0 + Duration::from_secs(1));
    assert_eq!(
        ops_since(&d.rig, from),
        ["install", "follow"],
        "no reveal, jump or open"
    );
    assert!(open_calls(&d.rig).is_empty());
    assert!(d.rig.layout.0.borrow().is_empty(), "the keys stay");
    for id in [reveal, jump, held] {
        assert_eq!(d.rig.page.replies_to(id).len(), 1, "answered once");
    }
}

#[test]
fn a_view_carries_the_document_prefixes_once_its_project_is_known() {
    let d = doc_base("host-doc-prefixes");
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    assert_eq!(d.rig.page.events("editor.view").len(), 1);
    assert_eq!(last_view(&d.rig)["doc"], Value::Null);
    d.rig
        .pump_until("the view again", |p| p.events("editor.view").len() == 2);
    let again = last_view(&d.rig);
    assert_eq!(again["tick"], 41);
    assert_eq!(
        again["doc"],
        json!({"root": "eitri-plugin://demo/.eitri/doc/1/", "base": "eitri-plugin://demo/.eitri/doc/1/docs/"})
    );
    assert_eq!(
        *d.slot.borrow(),
        DocSlot {
            generation: 1,
            dir: Some(d.proj.clone())
        }
    );
    d.rig.host.follow_view(named_view(&d.a, 3, 42), d.t0);
    assert_eq!(d.rig.page.events("editor.view").len(), 3);
    assert_eq!(last_view(&d.rig)["doc"], again["doc"], "known by its name at once");
    assert_eq!(d.slot.borrow().generation, 1);
}

#[test]
fn a_renamed_buffer_moves_the_document_route_to_its_new_project() {
    let d = doc_base("host-doc-renamed");
    let other = d.s.join("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    touch(&other.join("x.md"), "# x\n");
    d.rig.host.follow_view(named_view(&d.a, 3, 41), d.t0);
    wait_slot(&d, &d.proj);
    assert_eq!(d.slot.borrow().generation, 1);

    d.rig.host.follow_view(named_view(&other.join("x.md"), 3, 42), d.t0);
    assert_eq!(
        last_view(&d.rig)["doc"],
        Value::Null,
        "the new name's project is not known yet"
    );
    wait_slot(&d, &other);
    assert_eq!(
        *d.slot.borrow(),
        DocSlot {
            generation: 2,
            dir: Some(other.clone())
        }
    );
    let moved = last_view(&d.rig);
    assert_eq!(moved["tick"], 42);
    assert_eq!(
        moved["doc"],
        json!({"root": "eitri-plugin://demo/.eitri/doc/2/", "base": "eitri-plugin://demo/.eitri/doc/2/"})
    );

    d.rig.host.follow_view(named_view(&d.a, 3, 43), d.t0);
    assert_eq!(
        *d.slot.borrow(),
        DocSlot {
            generation: 3,
            dir: Some(d.proj.clone())
        }
    );
    assert_eq!(last_view(&d.rig)["doc"]["root"], "eitri-plugin://demo/.eitri/doc/3/");
}

#[test]
fn an_unnamed_buffer_has_no_prefixes_and_its_links_are_refused() {
    let d = doc_base("host-doc-unnamed");
    d.rig.host.follow_view(named_view(Path::new(""), 3, 41), d.t0);
    assert_eq!(last_view(&d.rig)["doc"], Value::Null);
    for _ in 0..10 {
        d.rig.host.pump(d.t0);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        d.rig.page.events("editor.view").len(),
        1,
        "no lookup, so no second view"
    );
    assert_eq!(*d.slot.borrow(), DocSlot::default());
    let refused = d.rig.reply(link(&d, "x.md"));
    assert_eq!(refused["error"]["code"], "outside_roots");
    assert_eq!(refused["error"]["message"], "the document has no file");
}

#[test]
fn a_replaced_documents_waiting_reveal_is_never_sent() {
    let d = shown("host-reveal-replaced");
    let t = d.t0 + ms(300);
    d.rig.host.follow_view(named_view(&d.a, 3, 42), t);
    d.rig.host.pump(t);
    let from = calls(&d.rig);
    let r = d.rig.send("editor.reveal", json!({"buf": 3, "line": 10, "token": 1}));
    d.rig.host.page_committed();
    answer(&d.rig, lines(3, 42, &["# a"]));
    d.rig.host.pump(t);
    d.rig.host.pump(t + Duration::from_secs(1));
    assert!(
        !ops_since(&d.rig, from).iter().any(|op| op == "reveal"),
        "{:?}",
        ops_since(&d.rig, from)
    );
    assert!(d.rig.page.replies_to(r).is_empty());
}
