//! The plugin host against a fake page, editor and layout, with the demo plugin's real (fake) CLI as the
//! task. Time is passed in where it decides anything; every real wait is a 60 s hang guard.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_OPEN_LUA};
use eitri_core::plugin::host::{LayoutPort, PagePort, PluginHost};
use eitri_core::plugin::load::load_plugin;
use eitri_core::plugin::protocol::FocusTarget;
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
}

impl EditorRpc for Editor {
    fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.calls.borrow_mut().push((code, args));
        self.answers.borrow_mut().push(answer);
        pending
    }
    fn target(&self) -> Option<u64> {
        Some(1)
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
        m.replacen(
            "[\"tasks\", \"editor.open\", \"editor.assist\", \"layout.focus\", \"state\", \"net\"]",
            "[\"tasks\", \"state\"]",
            1,
        )
        .replacen("[net]\norigins = [\"https://learn.example.org\"]\n", "", 1)
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
    rig.host.page_loading();
    let hello = rig.reply(rig.send("host.hello", json!({})));
    assert_eq!(
        hello["result"]["runs"],
        json!([{"run": run, "task": "watch", "vars": vars(), "tail": [line]}])
    );

    // Reloaded again and not yet ready: the end of the run is not sent to a page that has not said hello.
    rig.host.page_loading();
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
    rig.host.page_loading();
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
    rig.host.page_loading();
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
