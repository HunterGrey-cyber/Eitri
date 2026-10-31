//! Constant LSP chunks against an embedded nvim and in-process language servers; no sockets or display.
#[path = "support/embed_client.rs"]
mod embed_client;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;
use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_context::EditorContext;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
use eitri_core::editor_tools::stamp::{wall_ms, Sequencer, Stamp, EDITOR_BUDGET};
use eitri_core::editor_tools::step::ToolResult;
use eitri_core::editor_tools::tools::lsp::{self, AskArgs, Asked, Collected, LspKind, RawPart, UsagePart, LSP_BUDGET};
use eitri_core::editor_tools::way::{EditorWay, LocalWay};
use eitri_core::nvim_rpc::Pending;
use eitri_core::tabs::TabId;
use embed_client::{opts_map as map, Embed};
use rmpv::Value;
use serde_json::json;

const FAKE: &str = include_str!("support/fake_lsp.lua");
const OPEN: &str = "local p = ... local b = vim.fn.bufadd(p) vim.fn.bufload(b) return b";
const SET: &str =
    "local answers, hang, delay = ... _G.__fake_lsp = {seen = {}, answers = answers, hang = hang, delay = delay}";
const LOCATION: &str = "local p = ... return {uri = vim.uri_from_fname(p), range = {start = {line = 0, character = 2}, ['end'] = {line = 0, character = 3}}}";

struct Rpc(RefCell<Embed>);
impl EditorRpc for Rpc {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        answer.send(Ok(self.0.borrow_mut().lua(code, args)));
        pending
    }
    fn target(&self) -> Option<u64> {
        Some(1)
    }
}
/// One tab of a window whose editor is the test's nvim.
struct Env {
    way: LocalWay,
    root: PathBuf,
    declared: Rc<BTreeSet<String>>,
}
const TAB: TabId = TabId(1);
impl BrokerEnv for Env {
    fn way(&self) -> Option<&dyn EditorWay> {
        Some(&self.way)
    }
    fn facts(&self, tab: TabId) -> Option<TabFacts> {
        (tab == TAB).then(|| TabFacts {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            session: "session-1".to_owned(),
            process: "process-1".to_owned(),
            revision: 1,
            declared: self.declared.clone(),
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
struct Test {
    rpc: Rc<Rpc>,
    feed: EditorContextFeed,
    dir: PathBuf,
    root: PathBuf,
    path: PathBuf,
    buf: Value,
    seq: Sequencer,
}
impl Test {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "lsp-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let root = dir.join("project");
        let path = root.join("a.rs");
        std::fs::write(&path, "foo\n").unwrap();
        let feed = EditorContextFeed::new().expect("the feed must bind");
        let mut nvim = Embed::start_fed(&dir, &feed, &[]);
        let buf = nvim.lua(OPEN, vec![path.to_string_lossy().as_ref().into()]);
        let mut t = Self {
            rpc: Rc::new(Rpc(RefCell::new(nvim))),
            feed,
            dir,
            root,
            path,
            buf,
            seq: Sequencer::new("test".into()),
        };
        t.answers(Value::Map(vec![]), &[], 0);
        t
    }
    fn lua(&self, code: &str, args: Vec<Value>) -> Value {
        self.rpc.0.borrow_mut().lua(code, args)
    }
    fn args(&self) -> AskArgs {
        AskArgs {
            id: "one".into(),
            root: self.root.clone(),
            path: Some(self.path.clone()),
            line: Some(1),
            symbol: Some("foo".into()),
            budget_ms: 15000,
            ..Default::default()
        }
    }
    fn start(&self, name: &str, encoding: &str, buf: Value, root: &std::path::Path, caps: Option<Value>, delay: u32) {
        let mut config = vec![
            ("name", name.into()),
            ("encoding", encoding.into()),
            ("root", root.to_string_lossy().as_ref().into()),
            ("initialize_delay", delay.into()),
        ];
        if let Some(caps) = caps {
            config.push(("capabilities", caps));
        }
        self.lua(FAKE, vec![buf.clone(), map(&config)]);
        self.rpc.0.borrow_mut().wait_until("client attachment", |n| n.lua("local b, name = ... for _, c in ipairs(vim.lsp.get_clients({bufnr=b})) do if c.name == name then return true end end return false", vec![buf.clone(), name.into()]).as_bool() == Some(true));
        self.lua("_G.__fake_lsp.seen = {}", vec![]);
    }
    fn server(&self) {
        self.start("fake", "utf-16", self.buf.clone(), &self.root, None, 0);
    }
    fn answers(&mut self, answers: Value, hang: &[&str], delay: u32) {
        self.lua(
            SET,
            vec![
                answers,
                map(&hang.iter().map(|s| (*s, true.into())).collect::<Vec<_>>()),
                delay.into(),
            ],
        );
    }
    fn location(&self, path: &std::path::Path) -> Value {
        self.lua(LOCATION, vec![path.to_string_lossy().as_ref().into()])
    }
    fn definitions(&mut self, answer: Value, delay: u32) {
        self.answers(map(&[("textDocument/definition", answer)]), &[], delay);
    }
    fn ask(&mut self, args: &AskArgs) -> Asked {
        let stamp = self.seq.stamp(EDITOR_BUDGET);
        lsp::parse_asked(&lsp::ask(&*self.rpc, &stamp, args).try_take().unwrap().unwrap()).unwrap()
    }
    fn collect(&mut self, id: &str, last: bool) -> Collected {
        let stamp = self.seq.stamp(EDITOR_BUDGET);
        lsp::parse_collected(&lsp::collect(&*self.rpc, &stamp, id, last).try_take().unwrap().unwrap()).unwrap()
    }
    fn done(&mut self) -> Collected {
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            let value = self.collect("one", false);
            if !matches!(value, Collected::Pending) {
                return value;
            }
            assert!(Instant::now() < end, "server did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn parts(&mut self) -> Vec<RawPart> {
        match self.done() {
            Collected::Done { parts, .. } => parts,
            v => panic!("{v:?}"),
        }
    }
    fn ask_definitions(&mut self) {
        let mut a = self.args();
        a.include = vec![UsagePart::Definitions];
        assert!(matches!(self.ask(&a), Asked::Started { .. }));
    }
    fn seen(&self) -> Value {
        self.lua("return _G.__fake_lsp.seen", vec![])
    }
    /// One `usages` call through a real broker: each chunk stamped as a tab's, ticking until the agent is answered.
    fn usages(&self, arguments: &serde_json::Value) -> ToolResult {
        let env = Env {
            way: LocalWay(self.rpc.clone()),
            root: self.root.clone(),
            declared: Rc::new(BTreeSet::from(["usages".to_owned()])),
        };
        let mut broker = Broker::new();
        let caller = Caller::Sidecar {
            call_id: "call-1".to_owned(),
        };
        let text = arguments.to_string();
        let out = broker.submit(TAB, caller, None, "usages", &text, &env, Instant::now());
        assert!(out.is_empty(), "{out:?}");
        let end = Instant::now() + LSP_BUDGET * 2;
        loop {
            for out in broker.tick(&env, Instant::now()) {
                if let BrokerOut::Answer { result, .. } = out {
                    return result;
                }
            }
            assert!(Instant::now() < end, "the agent was not answered");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Test {
    fn drop(&mut self) {
        self.feed.cleanup();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A second buffer of the project, opened through a link to a file outside it, the link since replaced by a file.
fn read_from_outside(t: &Test) -> PathBuf {
    let outside = t.dir.join("outside.rs");
    std::fs::write(&outside, "secret line\n").unwrap();
    let name = t.root.join("b.rs");
    std::os::unix::fs::symlink(&outside, &name).unwrap();
    t.lua(OPEN, vec![name.to_string_lossy().as_ref().into()]);
    std::fs::remove_file(&name).unwrap();
    std::fs::write(&name, "plain\n").unwrap();
    name
}

#[test]
#[ignore]
fn an_item_in_a_buffer_read_from_outside_tells_no_line() {
    let mut t = Test::new();
    let name = read_from_outside(&t);
    t.server();
    t.definitions(t.location(&name), 0);
    let result = t.usages(&json!({"path": "a.rs", "line": 1, "symbol": "foo", "include": ["definitions"]}));
    assert!(!result.is_error, "{}", result.text);
    assert!(!result.text.contains("secret"), "{}", result.text);
    let told: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(
        told["definitions"][0]["path"], "b.rs",
        "the item keeps its place: {told}"
    );
    assert!(told["definitions"][0].get("text").is_none(), "{told}");
}

#[test]
#[ignore]
fn a_usages_call_on_a_buffer_read_from_outside_tells_nothing() {
    let mut t = Test::new();
    read_from_outside(&t);
    t.definitions(t.location(&t.path), 0);
    for symbol in ["secret", "absent"] {
        let result = t.usages(&json!({"path": "b.rs", "line": 1, "symbol": symbol}));
        assert!(result.is_error, "{}", result.text);
        assert!(
            result.text.contains("use Read"),
            "whether it is there is the buffer's text: {}",
            result.text
        );
    }
}

#[test]
#[ignore]
fn an_ask_answers_and_its_collect_takes_it_once() {
    let mut t = Test::new();
    t.server();
    t.definitions(t.location(&t.path), 0);
    t.ask_definitions();
    let parts = t.parts();
    assert_eq!(parts[0].items.len(), 1);
    assert!(matches!(t.collect("one", false), Collected::Gone));
}
#[test]
#[ignore]
fn pending_until_the_server_answers() {
    let mut t = Test::new();
    t.server();
    t.definitions(t.location(&t.path), 150);
    t.ask_definitions();
    assert!(matches!(t.collect("one", false), Collected::Pending));
    assert_eq!(t.parts()[0].items.len(), 1);
}
#[test]
#[ignore]
fn a_server_that_never_answers_gives_what_arrived_and_names_the_missing_part() {
    let mut t = Test::new();
    t.server();
    t.answers(
        map(&[("textDocument/definition", t.location(&t.path))]),
        &["textDocument/references"],
        0,
    );
    let mut a = t.args();
    a.include = vec![UsagePart::Definitions, UsagePart::References];
    t.ask(&a);
    assert_eq!(t.lua("for _, r in ipairs(_G.__fake_lsp.seen) do if r.method == 'textDocument/references' then return r.params.context.includeDeclaration end end return false", vec![]), true.into());
    std::thread::sleep(Duration::from_millis(30));
    match t.collect("one", true) {
        Collected::Done { parts, missing, .. } => {
            assert_eq!(parts[0].items.len(), 1);
            assert_eq!(missing, ["references"]);
        }
        v => panic!("{v:?}"),
    }
}
#[test]
#[ignore]
fn an_entry_past_its_deadline_is_cleared_by_the_next_ask() {
    let mut t = Test::new();
    t.server();
    let mut a = t.args();
    a.budget_ms = 1;
    t.ask(&a);
    std::thread::sleep(Duration::from_millis(20));
    a.id = "two".into();
    a.budget_ms = 1000;
    t.ask(&a);
    assert_eq!(
        t.lua("return _G.__eitri_agent.lsp.entries['test:one'] == nil", vec![]),
        true.into()
    );
    assert!(matches!(t.collect("one", true), Collected::Gone));
}
#[test]
#[ignore]
fn and_by_the_next_collect() {
    let mut t = Test::new();
    t.server();
    let mut a = t.args();
    a.budget_ms = 1;
    t.ask(&a);
    std::thread::sleep(Duration::from_millis(20));
    t.collect("another", false);
    assert_eq!(
        t.lua("return _G.__eitri_agent.lsp.entries['test:one'] == nil", vec![]),
        true.into()
    );
    assert!(matches!(t.collect("one", true), Collected::Gone));
}
#[test]
#[ignore]
fn a_last_collect_inside_the_margin_finds_the_entry() {
    let mut t = Test::new();
    t.server();
    t.answers(map(&[]), &["textDocument/definition"], 0);
    let mut a = t.args();
    a.budget_ms = lsp::COLLECT_MARGIN.as_millis() as u32 + 1;
    a.include = vec![UsagePart::Definitions];
    t.ask(&a);
    std::thread::sleep(Duration::from_millis(20));
    assert!(matches!(t.collect("one", true), Collected::Done { .. }));
}
#[test]
#[ignore]
fn a_character_across_each_cut_stays_whole() {
    let mut t = Test::new();
    t.server();
    let line = format!("{}€", "a".repeat(499));
    t.lua(
        "local b, s = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {s})",
        vec![t.buf.clone(), line.into()],
    );
    let hover = format!("{}€", "h".repeat(16383));
    let name = format!("{}€", "n".repeat(1023));
    let loc = t.location(&t.path);
    t.answers(
        map(&[
            ("textDocument/hover", map(&[("contents", hover.into())])),
            (
                "textDocument/documentSymbol",
                Value::Array(vec![map(&[
                    ("name", name.into()),
                    ("kind", 12.into()),
                    ("location", loc),
                ])]),
            ),
        ]),
        &[],
        0,
    );
    let mut a = t.args();
    a.symbol = None;
    a.kind = LspKind::Hover;
    t.ask(&a);
    match t.done() {
        Collected::Done {
            hover: Some(s), cut, ..
        } => {
            assert_eq!(s, "h".repeat(16383));
            assert!(cut, "a hover cut at its cap says so");
        }
        v => panic!("{v:?}"),
    }
    a.kind = LspKind::DocumentSymbols;
    t.ask(&a);
    let parts = t.parts();
    let item = &parts[0].items[0];
    assert_eq!(item.name.as_deref(), Some("n".repeat(1023).as_str()));
    assert_eq!(item.text.as_deref(), Some("a".repeat(499).as_str()));
}
#[test]
#[ignore]
fn a_line_of_invalid_utf8_does_not_fail_the_collect() {
    let mut t = Test::new();
    t.server();
    t.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {'foo' .. string.char(255)})",
        vec![t.buf.clone()],
    );
    t.definitions(t.location(&t.path), 0);
    let mut args = t.args();
    args.symbol = None;
    args.include = vec![UsagePart::Definitions];
    assert!(matches!(t.ask(&args), Asked::Started { .. }));
    assert_eq!(t.parts()[0].items[0].text.as_deref(), Some("foo�"));
}
#[test]
#[ignore]
fn a_very_long_name_and_path_are_held_to_their_caps() {
    let mut t = Test::new();
    t.server();
    let answer = t.lua("local p = ... return {{name=string.rep('n', 3000), containerName=string.rep('c', 3000), kind=12, location={uri=vim.uri_from_fname(p), range={start={line=0,character=0}}}}, {name='dropped', kind=12, location={uri='custom:' .. string.rep('x', 5000)}}}", vec![t.path.to_string_lossy().as_ref().into()]);
    t.answers(map(&[("textDocument/documentSymbol", answer)]), &[], 0);
    let mut a = t.args();
    a.kind = LspKind::DocumentSymbols;
    t.ask(&a);
    let parts = t.parts();
    assert_eq!(parts[0].total, 2);
    assert_eq!(parts[0].items.len(), 1);
    assert_eq!(parts[0].items[0].name.as_ref().unwrap().len(), 1024);
    assert_eq!(parts[0].items[0].container.as_ref().unwrap().len(), 1024);
}
#[test]
#[ignore]
fn a_position_a_server_got_wrong_drops_that_item_only() {
    let mut t = Test::new();
    t.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {'foo', 'two', 'three'})",
        vec![t.buf.clone()],
    );
    t.server();
    let answer = t.lua(
        "local p = ... local u = vim.uri_from_fname(p) local function at(l, c) return {uri=u, range={start={line=l, character=c}}} end return {at(0, -1), at(0.5, 0), at(0, 'x'), at(1e12, 0), at(2, 1), at(0/0, 0)}",
        vec![t.path.to_string_lossy().as_ref().into()],
    );
    t.definitions(answer, 0);
    t.ask_definitions();
    let p = t.parts();
    assert_eq!(p[0].total, 6);
    assert_eq!(p[0].items.len(), 1);
    assert_eq!((p[0].items[0].line, p[0].items[0].character), (3, 1));
    assert_eq!(p[0].items[0].text.as_deref(), Some("three"));
}
#[test]
#[ignore]
fn a_late_answer_after_the_entry_went_recreates_nothing() {
    let mut t = Test::new();
    t.server();
    t.definitions(t.location(&t.path), 100);
    t.ask_definitions();
    assert!(matches!(t.collect("one", true), Collected::Done { .. }));
    std::thread::sleep(Duration::from_millis(150));
    assert!(matches!(t.collect("one", false), Collected::Gone));
    assert_eq!(t.lua("return _G.__eitri_agent.lsp.n", vec![]), 0.into());
}
#[test]
#[ignore]
fn an_items_text_names_the_buffer_it_came_from() {
    let mut t = Test::new();
    t.server();
    let outside = t.dir.join("outside.rs");
    std::fs::write(&outside, "secret\n").unwrap();
    t.lua(OPEN, vec![outside.to_string_lossy().as_ref().into()]);
    let link = t.root.join("link.rs");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    t.definitions(t.location(&link), 0);
    t.ask_definitions();
    let parts = t.parts();
    assert_eq!(parts[0].items[0].text.as_deref(), Some("secret"));
    assert_eq!(parts[0].items[0].text_from.as_ref(), Some(&outside));
}
#[test]
#[ignore]
fn the_editor_is_never_held() {
    let mut t = Test::new();
    t.server();
    t.answers(map(&[]), &["textDocument/definition"], 0);
    t.lua("local b = ... vim.api.nvim_set_current_buf(b)", vec![t.buf.clone()]);
    t.rpc.0.borrow_mut().input("iunsaved ");
    const PERSON: &str = "return {vim.api.nvim_get_current_win(), vim.api.nvim_get_mode().mode, vim.api.nvim_win_get_cursor(0), vim.api.nvim_buf_get_lines(0, 0, -1, true)}";
    let before = t.lua(PERSON, vec![]);
    assert_eq!(before.as_array().unwrap()[1], Value::from("i"));
    assert_eq!(before.as_array().unwrap()[3], Value::Array(vec!["unsaved foo".into()]));
    // The clock runs across the ask and the mode request after it: a hold inside the chunk shows in the first, and
    // one it leaves scheduled shows in the second. The bound is a multiple of an empty round trip on this machine,
    // never a number of milliseconds. The fastest of three tries counts, so one preemption of this process does not
    // read as a hold, while a real hold is in all three.
    let mut warm = t.args();
    warm.id = "warm".into();
    warm.include = vec![UsagePart::Definitions];
    assert!(matches!(t.ask(&warm), Asked::Started { .. }));
    let mut round_trips: Vec<Duration> = (0..9)
        .map(|_| {
            let start = Instant::now();
            t.lua("return 1", vec![]);
            start.elapsed()
        })
        .collect();
    round_trips.sort();
    let round_trip = round_trips[round_trips.len() / 2];
    let held = (0..3)
        .map(|try_no| {
            let mut timed = t.args();
            timed.id = format!("timed{try_no}");
            timed.include = vec![UsagePart::Definitions];
            let start = Instant::now();
            assert!(matches!(t.ask(&timed), Asked::Started { .. }));
            let mode = t.rpc.0.borrow_mut().request("nvim_get_mode", vec![]);
            let held = start.elapsed();
            assert_eq!(
                mode.as_map()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("blocking"))
                    .unwrap()
                    .1,
                false.into()
            );
            held
        })
        .min()
        .unwrap();
    assert!(
        held < round_trip * 2000,
        "an ask and a mode request took {held:?} against an empty round trip of {round_trip:?}"
    );
    assert_eq!(before, t.lua(PERSON, vec![]));
}
#[test]
#[ignore]
fn a_position_is_in_each_clients_encoding() {
    let mut t = Test::new();
    t.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {'😀 foo'})",
        vec![t.buf.clone()],
    );
    for enc in ["utf-8", "utf-16", "utf-32"] {
        t.start(enc, enc, t.buf.clone(), &t.root, None, 0);
    }
    t.ask_definitions();
    t.parts();
    let got = t.lua("local out = {} for _, r in ipairs(_G.__fake_lsp.seen) do out[r.client] = r.params.position.character end return out", vec![]);
    for (enc, expected) in [("utf-8", 5), ("utf-16", 3), ("utf-32", 2)] {
        assert_eq!(
            got.as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some(enc))
                .unwrap()
                .1,
            expected.into()
        );
    }
}
#[test]
#[ignore]
fn the_symbol_is_a_whole_word() {
    let mut t = Test::new();
    t.server();
    t.lua(
        "local b = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {'foobar fooé foo'})",
        vec![t.buf.clone()],
    );
    t.ask_definitions();
    t.parts();
    assert_eq!(
        t.lua("return _G.__fake_lsp.seen[1].params.position.character", vec![]),
        12.into()
    );
}
#[test]
#[ignore]
fn a_symbol_not_on_the_line_says_so() {
    let mut t = Test::new();
    t.server();
    let mut a = t.args();
    a.symbol = Some("absent".into());
    assert!(matches!(t.ask(&a), Asked::NoSymbol));
    assert!(t.seen().as_array().unwrap().is_empty());
}
#[test]
#[ignore]
fn a_line_past_the_end_says_so() {
    let mut t = Test::new();
    let mut a = t.args();
    a.line = Some(2);
    assert!(matches!(t.ask(&a), Asked::NoLine { line_count: 1 }));
}
#[test]
#[ignore]
fn load_false_adds_no_buffer() {
    let mut t = Test::new();
    let path = t.root.join("new.rs");
    std::fs::write(&path, "foo").unwrap();
    let before = t.lua("return vim.api.nvim_list_bufs()", vec![]);
    let mut a = t.args();
    a.path = Some(path);
    assert!(matches!(t.ask(&a), Asked::NeedsLoad));
    assert_eq!(before, t.lua("return vim.api.nvim_list_bufs()", vec![]));
}
#[test]
#[ignore]
fn a_stale_swap_loads_nothing_and_says_swap() {
    let mut t = Test::new();
    let path = t.root.join("swap.rs");
    std::fs::write(&path, "foo\n").unwrap();
    let swaps = t.dir.join("swaps");
    std::fs::create_dir(&swaps).unwrap();
    const SWAP_OPTIONS: &str = "local d = ... vim.o.directory=d vim.o.updatecount=200 vim.o.swapfile=true";
    let swap;
    {
        let mut crashed = Embed::start(&t.dir.join("crashed"), &[], &[]);
        crashed.lua(SWAP_OPTIONS, vec![swaps.to_string_lossy().as_ref().into()]);
        let buf = crashed.lua(OPEN, vec![path.to_string_lossy().as_ref().into()]);
        swap = crashed.lua(
            "local b = ... vim.api.nvim_set_current_buf(b) vim.api.nvim_buf_set_lines(b, 0, -1, true, {'unsaved'}) vim.cmd('preserve') return vim.fn.swapname(b)",
            vec![buf],
        );
    }
    let bytes = std::fs::read(swap.as_str().unwrap()).unwrap();
    t.lua(SWAP_OPTIONS, vec![swaps.to_string_lossy().as_ref().into()]);
    let before = t.lua("return vim.api.nvim_list_bufs()", vec![]);
    let mut a = t.args();
    a.path = Some(path);
    a.load = true;
    assert!(matches!(t.ask(&a), Asked::Swap));
    assert_eq!(before, t.lua("return vim.api.nvim_list_bufs()", vec![]));
    assert_eq!(std::fs::read(swap.as_str().unwrap()).unwrap(), bytes);
}
#[test]
#[ignore]
fn a_load_says_loaded_now_and_a_later_ask_finds_the_client() {
    let mut t = Test::new();
    let path = t.root.join("new.rs");
    std::fs::write(&path, "foo\n").unwrap();
    t.lua("local src, root = ... vim.api.nvim_create_autocmd('BufReadPost', {callback=function(ev) assert(loadstring(src))(ev.buf, {name='loaded',root=root,initialize_delay=50}) end})", vec![FAKE.into(),t.root.to_string_lossy().as_ref().into()]);
    let mut a = t.args();
    a.path = Some(path);
    a.load = true;
    assert!(matches!(t.ask(&a), Asked::NoClient { loaded_now: true }));
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(t.ask(&a), Asked::Started { .. }));
}
#[test]
#[ignore]
fn no_client_says_no_client() {
    let mut t = Test::new();
    let a = t.args();
    assert!(matches!(t.ask(&a), Asked::NoClient { loaded_now: false }));
}
#[test]
#[ignore]
fn an_expired_stamp_starts_no_request() {
    let t = Test::new();
    t.server();
    let a = t.args();
    let stamp = Stamp {
        scope: "expired".into(),
        seq: 1,
        deadline_ms: wall_ms() - 1,
    };
    let value = lsp::ask(&*t.rpc, &stamp, &a).try_take().unwrap().unwrap();
    assert_eq!(value, map(&[("expired", true.into())]));
    assert!(t.seen().as_array().unwrap().is_empty());
}
#[test]
#[ignore]
fn two_scopes_do_not_share_an_id() {
    let mut t = Test::new();
    t.server();
    t.ask_definitions();
    t.seq = Sequencer::new("other".into());
    assert!(matches!(t.collect("one", true), Collected::Gone));
    t.ask_definitions();
    t.parts();
    t.seq = Sequencer::new("test".into());
    let _ = t.seq.stamp(EDITOR_BUDGET);
    assert!(matches!(t.collect("one", true), Collected::Done { .. }));
}
#[test]
#[ignore]
fn every_location_form_normalises() {
    let mut t = Test::new();
    t.server();
    let loc = t.location(&t.path);
    let link = t.lua(
        "local l = ... return {targetUri=l.uri,targetSelectionRange={start={line=2,character=7}},targetRange=l.range}",
        vec![loc.clone()],
    );
    for (answer, expected) in [
        (loc.clone(), (1, 2)),
        (Value::Array(vec![loc.clone()]), (1, 2)),
        (Value::Array(vec![link]), (3, 7)),
    ] {
        t.definitions(answer, 0);
        t.ask_definitions();
        let p = t.parts();
        assert_eq!(p[0].items.len(), 1);
        assert_eq!(p[0].items[0].path, t.path.to_string_lossy());
        assert_eq!((p[0].items[0].line, p[0].items[0].character), expected);
    }
}
#[test]
#[ignore]
fn a_non_file_uri_stays_as_it_came_and_adds_no_buffer() {
    let mut t = Test::new();
    t.server();
    let before = t.lua("return vim.api.nvim_list_bufs()", vec![]);
    let loc = t.lua(
        "return {uri='rust-analyzer://crate/source',range={start={line=4,character=9}}}",
        vec![],
    );
    t.definitions(loc, 0);
    t.ask_definitions();
    let p = t.parts();
    assert_eq!(p[0].items[0].path, "rust-analyzer://crate/source");
    assert_eq!((p[0].items[0].line, p[0].items[0].character), (5, 9));
    assert_eq!(before, t.lua("return vim.api.nvim_list_bufs()", vec![]));
}
#[test]
#[ignore]
fn the_cap_keeps_200_and_counts_the_rest() {
    let mut t = Test::new();
    t.server();
    let answer = t.lua(
        "local l = ... local out = {} for i=1,230 do out[i]=l end return out",
        vec![t.location(&t.path)],
    );
    t.definitions(answer, 0);
    t.ask_definitions();
    let p = t.parts();
    assert_eq!(p[0].items.len(), 200);
    assert_eq!(p[0].total, 230);
}
#[test]
#[ignore]
fn every_hover_form_comes_as_its_source_text() {
    let mut t = Test::new();
    t.server();
    for (contents, expected) in [
        ("**source**".into(), "**source**"),
        (
            map(&[("kind", "markdown".into()), ("value", "# title".into())]),
            "# title",
        ),
        (
            map(&[("language", "rust".into()), ("value", "fn foo()".into())]),
            "fn foo()",
        ),
        (
            Value::Array(vec!["first".into(), map(&[("value", "second".into())])]),
            "first\n\nsecond",
        ),
    ] {
        t.answers(map(&[("textDocument/hover", map(&[("contents", contents)]))]), &[], 0);
        let mut a = t.args();
        a.kind = LspKind::Hover;
        t.ask(&a);
        match t.done() {
            Collected::Done { hover: Some(s), .. } => assert_eq!(s, expected),
            v => panic!("{v:?}"),
        }
    }
}
#[test]
#[ignore]
fn document_symbols_flatten_with_their_container() {
    let mut t = Test::new();
    t.server();
    let answer=t.lua("return {{name='Parent',kind=5,range={start={line=0,character=0}},selectionRange={start={line=1,character=2}},children={{name='child',kind=6,selectionRange={start={line=3,character=4}}}}}}",vec![]);
    t.answers(map(&[("textDocument/documentSymbol", answer)]), &[], 0);
    let mut a = t.args();
    a.kind = LspKind::DocumentSymbols;
    t.ask(&a);
    let p = t.parts();
    assert_eq!(p[0].items.len(), 2);
    assert_eq!(p[0].items[0].kind.as_deref(), Some("Class"));
    assert_eq!(p[0].items[1].container.as_deref(), Some("Parent"));
    assert_eq!(p[0].items[1].line, 4);
}
#[test]
#[ignore]
fn workspace_symbols_ask_only_clients_of_the_project() {
    let mut t = Test::new();
    t.server();
    let outside = t.dir.join("out.rs");
    std::fs::write(&outside, "foo").unwrap();
    let b = t.lua(OPEN, vec![outside.to_string_lossy().as_ref().into()]);
    t.start("outside", "utf-16", b, &t.dir, None, 0);
    let answer = t.lua(
        "local p = ... return {{name='found',kind=12,containerName='crate',location={uri=vim.uri_from_fname(p)}}}",
        vec![t.path.to_string_lossy().as_ref().into()],
    );
    t.answers(map(&[("workspace/symbol", answer)]), &[], 0);
    let mut a = t.args();
    a.kind = LspKind::WorkspaceSymbols;
    a.path = None;
    a.line = None;
    a.symbol = None;
    a.query = Some("needle".into());
    t.ask(&a);
    let p = t.parts();
    assert_eq!(p[0].items[0].line, 1);
    assert_eq!(p[0].items[0].container.as_deref(), Some("crate"));
    assert_eq!(t.lua("return #_G.__fake_lsp.seen == 1 and _G.__fake_lsp.seen[1].client == 'fake' and _G.__fake_lsp.seen[1].params.query == 'needle'",vec![]),true.into());
}
#[test]
#[ignore]
fn an_unsupported_part_is_named() {
    let mut t = Test::new();
    t.start(
        "partial",
        "utf-16",
        t.buf.clone(),
        &t.root,
        Some(map(&[("definitionProvider", true.into())])),
        0,
    );
    let mut a = t.args();
    match t.ask(&a) {
        Asked::Started { unsupported, .. } => assert_eq!(unsupported, ["references", "implementations"]),
        v => panic!("{v:?}"),
    }
    let p = t.parts();
    assert_eq!(p.len(), 1);
    let before = t.seen();
    a.kind = LspKind::Hover;
    assert!(matches!(t.ask(&a), Asked::NoClient { loaded_now: false }));
    assert_eq!(before, t.seen());
}

#[test]
#[ignore]
fn the_oldest_entry_is_evicted_after_thirty_two() {
    let mut t = Test::new();
    t.server();
    t.answers(map(&[]), &["textDocument/definition"], 0);
    for i in 0..33 {
        let mut args = t.args();
        args.id = format!("entry-{i}");
        args.include = vec![UsagePart::Definitions];
        assert!(matches!(t.ask(&args), Asked::Started { .. }));
    }
    assert_eq!(t.lua("return _G.__eitri_agent.lsp.n", vec![]), 32.into());
    assert!(matches!(t.collect("entry-0", true), Collected::Gone));
    assert!(matches!(t.collect("entry-1", true), Collected::Done { .. }));
    assert!(matches!(t.collect("entry-32", true), Collected::Done { .. }));
}
#[test]
#[ignore]
fn usages_with_a_hung_part_answers_what_arrived_when_the_budget_ends() {
    let mut t = Test::new();
    t.server();
    t.answers(
        map(&[("textDocument/definition", t.location(&t.path))]),
        &["textDocument/references"],
        0,
    );
    let began = Instant::now();
    let result = t.usages(&json!({"path": "a.rs", "line": 1, "symbol": "foo"}));
    let waited = began.elapsed();
    assert!(!result.is_error, "{}", result.text);
    let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(answer["definitions"][0]["path"], "a.rs");
    assert_eq!(answer["definitions"][0]["line"], 1);
    assert_eq!(answer["definitions"][0]["text"], "foo");
    assert_eq!(answer["missing"], json!(["references"]));
    assert!(answer.get("references").is_none(), "{answer}");
    assert!(waited >= LSP_BUDGET, "the budget was not given: {waited:?}");
}
#[test]
#[ignore]
fn an_unsaved_new_file_reaches_its_server() {
    let mut t = Test::new();
    let new = t.root.join("new.rs");
    let buf = t.lua(
        "local p = ... local b = vim.api.nvim_create_buf(true, false) vim.api.nvim_buf_set_name(b, p) \
         vim.api.nvim_buf_set_lines(b, 0, -1, true, {'foo unsaved'}) return b",
        vec![new.to_string_lossy().as_ref().into()],
    );
    let root = t.root.clone();
    t.start("new-file", "utf-16", buf, &root, None, 0);
    t.definitions(t.location(&new), 0);
    let result = t.usages(&json!({"path": "new.rs", "line": 1, "symbol": "foo", "include": ["definitions"]}));
    assert!(!result.is_error, "{}", result.text);
    let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(answer["definitions"][0]["path"], "new.rs");
    assert_eq!(answer["definitions"][0]["text"], "foo unsaved");
    assert!(!new.exists(), "nothing was written");
}

impl Test {
    fn edit_server(&self) {
        self.start(
            "edits",
            "utf-16",
            self.buf.clone(),
            &self.root,
            Some(map(&[
                ("renameProvider", true.into()),
                ("documentFormattingProvider", true.into()),
                ("codeActionProvider", map(&[("resolveProvider", true.into())])),
            ])),
            0,
        );
    }
    fn edit_args(&self, kind: LspKind) -> AskArgs {
        AskArgs {
            kind,
            new_name: Some("bar".into()),
            ..self.args()
        }
    }
    fn workspace_edit(&self, documents: bool) -> Value {
        self.lua("local p, documents = ... local edits = {{range={start={line=0,character=0}, ['end']={line=0,character=3}}, newText='bar', annotationId='note'}} if documents then return {documentChanges={{textDocument={uri=vim.uri_from_fname(p),version=1},edits=edits}}} end return {changes={[vim.uri_from_fname(p)]=edits}}", vec![self.path.to_string_lossy().as_ref().into(), documents.into()])
    }
    fn edit_done(&mut self) -> lsp::RawEdit {
        match self.done() {
            Collected::Done { edit: Some(edit), .. } => edit,
            other => panic!("{other:?}"),
        }
    }
    fn list_action(&mut self, body: &str) -> lsp::RawAction {
        self.edit_server();
        let action = self.lua(body, vec![self.workspace_edit(false)]);
        self.answers(map(&[("textDocument/codeAction", Value::Array(vec![action]))]), &[], 0);
        assert!(matches!(
            self.ask(&self.edit_args(LspKind::CodeActions)),
            Asked::Started { .. }
        ));
        match self.done() {
            Collected::Done { mut actions, .. } => {
                assert_eq!(actions.len(), 1);
                actions.remove(0)
            }
            other => panic!("{other:?}"),
        }
    }
    fn action_args(&self, id: &str) -> AskArgs {
        AskArgs {
            action: Some(id.into()),
            path: None,
            line: None,
            symbol: None,
            ..self.edit_args(LspKind::ActionEdit)
        }
    }
}

#[test]
#[ignore]
fn a_rename_comes_back_as_files_and_edits() {
    let mut t = Test::new();
    t.edit_server();
    t.answers(map(&[("textDocument/rename", t.workspace_edit(false))]), &[], 0);
    assert!(matches!(t.ask(&t.edit_args(LspKind::Rename)), Asked::Started { .. }));
    let edit = t.edit_done();
    assert_eq!(edit.encoding, eitri_core::editor_tools::text_edits::Encoding::Utf16);
    assert_eq!(edit.files.len(), 1);
    assert_eq!(edit.files[0].path, t.path.to_string_lossy());
    assert_eq!(edit.files[0].edits[0].start, (0, 0));
    assert_eq!(edit.files[0].edits[0].end, (0, 3));
    assert_eq!(edit.files[0].edits[0].new_text, "bar");
    assert_eq!(
        t.lua("return _G.__fake_lsp.seen[1].params.newName", vec![]),
        "bar".into()
    );
    assert_eq!(
        t.lua(
            "local b=... return vim.api.nvim_buf_get_lines(b,0,-1,true)[1]",
            vec![t.buf.clone()]
        ),
        "foo".into()
    );
}
#[test]
#[ignore]
fn both_workspace_edit_forms_normalise() {
    for documents in [false, true] {
        let mut t = Test::new();
        t.edit_server();
        t.answers(map(&[("textDocument/rename", t.workspace_edit(documents))]), &[], 0);
        t.ask(&t.edit_args(LspKind::Rename));
        let edit = t.edit_done();
        assert_eq!(edit.files.len(), 1);
        assert_eq!(edit.files[0].edits[0].new_text, "bar");
    }
    let mut t = Test::new();
    t.edit_server();
    let edit = t.lua(
        "local e=... e.documentChanges[2]=vim.deepcopy(e.documentChanges[1]) return e",
        vec![t.workspace_edit(true)],
    );
    t.answers(map(&[("textDocument/rename", edit)]), &[], 0);
    t.ask(&t.edit_args(LspKind::Rename));
    let edit = t.edit_done();
    assert_eq!(edit.files.len(), 2);
    assert_eq!(edit.files[0].path, edit.files[1].path);
}
#[test]
#[ignore]
fn a_resource_operation_is_named_and_not_lost() {
    let mut t = Test::new();
    t.edit_server();
    let edit=t.lua("local p=... return {documentChanges={{kind='rename',oldUri=vim.uri_from_fname(p),newUri=vim.uri_from_fname(p..'.new')}}}",vec![t.path.to_string_lossy().as_ref().into()]);
    t.answers(map(&[("textDocument/rename", edit)]), &[], 0);
    t.ask(&t.edit_args(LspKind::Rename));
    let edit = t.edit_done();
    assert_eq!(edit.resource_ops.len(), 1);
    assert!(edit.resource_ops[0].contains("rename"));
    assert!(edit.resource_ops[0].contains("a.rs"));
}
#[test]
#[ignore]
fn an_oversized_edit_says_so_and_keeps_none() {
    for mode in ["count", "text", "path", "entries", "operations"] {
        let mut t = Test::new();
        t.edit_server();
        let edit=t.lua("local e,mode=... if mode=='count' then for i=2,20001 do e.documentChanges[1].edits[i]=e.documentChanges[1].edits[1] end elseif mode=='text' then e.documentChanges[1].edits[1].newText=string.rep('x',8388609) elseif mode=='entries' then for i=2,20001 do e.documentChanges[i]={textDocument=e.documentChanges[1].textDocument,edits={}} end elseif mode=='operations' then for i=2,20001 do e.documentChanges[i]={kind='create',uri=e.documentChanges[1].textDocument.uri} end else e.documentChanges[1].textDocument.uri='file:///'..string.rep('x',4097) end return e",vec![t.workspace_edit(true),mode.into()]);
        t.answers(map(&[("textDocument/rename", edit)]), &[], 0);
        t.ask(&t.edit_args(LspKind::Rename));
        let edit = t.edit_done();
        assert!(edit.too_large);
        assert!(edit.files.is_empty());
        assert!(edit.resource_ops.is_empty());
    }
}
/// The whole edit goes back in one frame of the editor's connection, so names count with the text: 5,000 entries of
/// 4 KB names are over the 16 MiB a frame may hold, and a frame over it would end the connection.
#[test]
#[ignore]
fn names_too_long_for_the_editors_frame_say_so_and_keep_none() {
    for entry in [
        "{textDocument={uri=name,version=1},edits={}}",
        "{kind='create',uri=name}",
    ] {
        let mut t = Test::new();
        t.edit_server();
        t.answers(map(&[("textDocument/rename", t.workspace_edit(true))]), &[], 0);
        let many = format!(
            "local changes=_G.__fake_lsp.answers['textDocument/rename'].documentChanges \
             for i=1,5000 do local name='file:///'..string.rep('x',4000)..i changes[i]={entry} end"
        );
        t.lua(&many, vec![]);
        t.ask(&t.edit_args(LspKind::Rename));
        let edit = t.edit_done();
        assert!(edit.too_large, "{entry}");
        assert!(edit.files.is_empty(), "{entry}");
        assert!(edit.resource_ops.is_empty(), "{entry}");
    }
}
#[test]
#[ignore]
fn a_rename_asks_one_client() {
    let mut t = Test::new();
    t.edit_server();
    t.server();
    t.start(
        "second",
        "utf-8",
        t.buf.clone(),
        &t.root,
        Some(map(&[("renameProvider", true.into())])),
        0,
    );
    t.answers(map(&[("textDocument/rename", t.workspace_edit(false))]), &[], 0);
    t.ask(&t.edit_args(LspKind::Rename));
    t.edit_done();
    assert_eq!(t.lua("local n=0 for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='textDocument/rename' then n=n+1 end end return n",vec![]),1.into());
}
#[test]
#[ignore]
fn format_uses_the_target_buffers_options_not_the_current_ones() {
    let mut t = Test::new();
    t.edit_server();
    for width in [0, 3] {
        t.lua("local b,sw=... vim.bo[b].shiftwidth=sw vim.bo[b].tabstop=7 vim.bo[b].expandtab=true local other=vim.api.nvim_create_buf(true,false) vim.api.nvim_set_current_buf(other) vim.bo[other].shiftwidth=2 vim.bo[other].expandtab=false",vec![t.buf.clone(),width.into()]);
        let edits = t.lua(
            "local e=... for _,v in pairs(e.changes) do return v end",
            vec![t.workspace_edit(false)],
        );
        t.answers(map(&[("textDocument/formatting", edits)]), &[], 0);
        let mut a = t.edit_args(LspKind::Format);
        a.symbol = None;
        a.line = None;
        t.ask(&a);
        t.edit_done();
        assert_eq!(
            t.lua(
                "local o=_G.__fake_lsp.seen[1].params.options return {o.tabSize,o.insertSpaces}",
                vec![]
            ),
            Value::Array(vec![if width == 0 { 7.into() } else { 3.into() }, true.into()])
        );
    }
}
#[test]
#[ignore]
fn a_null_edit_is_no_edit() {
    for kind in [LspKind::Rename, LspKind::Format] {
        let mut t = Test::new();
        t.edit_server();
        t.ask(&t.edit_args(kind));
        assert!(matches!(t.done(), Collected::Done { edit: None, .. }));
    }
}
#[test]
#[ignore]
fn code_actions_list_id_and_title_and_keep_the_action_in_nvim() {
    let mut t = Test::new();
    let action = t.list_action("local e=... return {title='Fix this',kind='quickfix',edit=e,data={secret='kept'}}");
    assert_eq!(action.title, "Fix this");
    assert_eq!(action.kind.as_deref(), Some("quickfix"));
    assert!(action.has_edit);
    assert!(!action.has_command);
    assert_eq!(
        t.lua(
            "local id=... return _G.__eitri_agent.lsp.actions['test:'..id].action.data.secret",
            vec![action.id.into()]
        ),
        "kept".into()
    );
}
/// What a server gets back in a code action's context can come out in the action's title.
#[test]
#[ignore]
fn a_code_action_goes_out_with_no_entry_that_stood_through_a_reload() {
    let mut t = Test::new();
    t.lua(
        "local b = ...
         vim.diagnostic.set(vim.api.nvim_create_namespace('server'), b, {
           { lnum = 0, col = 0, message = 'stood', user_data = { lsp = { message = 'stood' } } },
         })
         vim.b[b].eitri_read_from = '/a/file/read/before'
         vim.api.nvim_buf_call(b, function() vim.cmd('edit!') end)",
        vec![t.buf.clone()],
    );
    t.edit_server();
    t.answers(map(&[("textDocument/codeAction", Value::Array(vec![]))]), &[], 0);
    let asked = t.ask(&t.edit_args(LspKind::CodeActions));
    assert!(matches!(asked, Asked::Started { .. }), "{asked:?}");
    t.done();
    assert_eq!(
        t.lua(
            "for _, r in ipairs(_G.__fake_lsp.seen) do
               if r.method == 'textDocument/codeAction' then return #r.params.context.diagnostics end
             end
             return -1",
            vec![]
        ),
        0.into()
    );
}

#[test]
#[ignore]
fn the_range_is_in_each_clients_encoding() {
    let mut t = Test::new();
    t.lua(
        "local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'é𝒳x'})",
        vec![t.buf.clone()],
    );
    for encoding in ["utf-8", "utf-16", "utf-32"] {
        t.start(
            encoding,
            encoding,
            t.buf.clone(),
            &t.root,
            Some(map(&[("codeActionProvider", true.into())])),
            0,
        );
    }
    let mut a = t.edit_args(LspKind::CodeActions);
    a.symbol = None;
    t.ask(&a);
    t.done();
    assert_eq!(t.lua("local out={} for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='textDocument/codeAction' then out[r.client]=r.params.range['end'].character assert(r.params.range.start.character==0) end end return {out['utf-8'],out['utf-16'],out['utf-32']}",vec![]),Value::Array(vec![7.into(),4.into(),3.into()]));
}
#[test]
#[ignore]
fn the_lines_lsp_diagnostics_go_along() {
    let mut t = Test::new();
    t.edit_server();
    t.lua("local b=... local ns=vim.api.nvim_create_namespace('diagnostics') vim.diagnostic.set(ns,b,{{lnum=0,col=0,message='shown',user_data={lsp={message='raw',code=42}}},{lnum=0,col=1,message='local'},{lnum=1,col=0,message='other',user_data={lsp={message='other'}}}})",vec![t.buf.clone()]);
    let mut a = t.edit_args(LspKind::CodeActions);
    a.symbol = None;
    t.ask(&a);
    t.done();
    assert_eq!(t.lua("local c=_G.__fake_lsp.seen[1].params.context return c.triggerKind==1 and #c.diagnostics==1 and c.diagnostics[1].message=='raw' and c.diagnostics[1].code==42",vec![]),true.into());
}
#[test]
#[ignore]
fn a_bare_command_says_has_command() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Run',command='server.run',arguments={'arg'}}");
    assert!(a.has_command);
    assert!(!a.has_edit);
}
#[test]
#[ignore]
fn a_bare_command_is_not_sent_to_resolve() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Run',command='server.run',arguments={'arg'}}");
    assert_eq!(t.ask(&t.action_args(&a.id)), Asked::NoEdit { has_command: true });
    assert_eq!(t.lua("for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='codeAction/resolve' then return true end end return false",vec![]),false.into());
}
#[test]
#[ignore]
fn a_command_that_only_the_resolved_action_has_reaches_done() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Fix',data={token=42}}");
    let resolved = t.lua(
        "local e=... return {title='Fix',edit=e,command={command='run'}}",
        vec![t.workspace_edit(true)],
    );
    t.answers(map(&[("codeAction/resolve", resolved)]), &[], 0);
    assert!(matches!(t.ask(&t.action_args(&a.id)), Asked::Started { action: Some(facts), .. } if !facts.has_command));
    match t.done() {
        Collected::Done {
            edit: Some(edit),
            action: Some(facts),
            ..
        } => {
            assert_eq!(edit.files.len(), 1);
            assert!(facts.has_command);
        }
        other => panic!("{other:?}"),
    }
}
#[test]
#[ignore]
fn action_edit_uses_the_actions_own_edit() {
    let mut t = Test::new();
    let a = t.list_action("local e=... return {title='Fix',edit=e}");
    assert!(matches!(t.ask(&t.action_args(&a.id)), Asked::Started { .. }));
    assert_eq!(t.edit_done().files[0].edits[0].new_text, "bar");
    assert_eq!(t.lua("for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='codeAction/resolve' then return true end end return false",vec![]),false.into());
}
#[test]
#[ignore]
fn a_stored_action_answers_the_time_of_its_first_ask() {
    let mut t = Test::new();
    let before = wall_ms();
    let a = t.list_action("local e=... return {title='Fix',edit=e}");
    std::thread::sleep(Duration::from_millis(20));
    let later = wall_ms();
    assert!(matches!(t.ask(&t.action_args(&a.id)), Asked::Started { .. }));
    match t.done() {
        Collected::Done { asked_ms, .. } => {
            let asked = asked_ms.expect("the time of the ask");
            assert!(before <= asked && asked <= later, "{before} {asked} {later}");
        }
        other => panic!("{other:?}"),
    }
}
#[test]
#[ignore]
fn action_edit_resolves_when_it_has_none() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Fix',data={token=42}}");
    let resolved = t.lua("local e=... return {title='Fix',edit=e}", vec![t.workspace_edit(true)]);
    t.answers(map(&[("codeAction/resolve", resolved)]), &[], 0);
    t.ask(&t.action_args(&a.id));
    assert_eq!(t.edit_done().files.len(), 1);
    assert_eq!(
        t.lua("return _G.__fake_lsp.seen[1].params.data.token", vec![]),
        42.into()
    );
}
#[test]
#[ignore]
fn action_edit_with_neither_says_no_edit() {
    let mut t = Test::new();
    let a = t.list_action("return {title='No edit'}");
    t.lua("for _,c in ipairs(vim.lsp.get_clients()) do c.server_capabilities.codeActionProvider={resolveProvider=false} end",vec![]);
    assert_eq!(t.ask(&t.action_args(&a.id)), Asked::NoEdit { has_command: false });
}
#[test]
#[ignore]
fn action_edit_says_its_title_and_command() {
    let mut t = Test::new();
    let a = t.list_action("local e=... return {title='Fix then run',edit=e,command={command='run'}}");
    let expected = lsp::ActionFacts {
        title: "Fix then run".into(),
        has_command: true,
    };
    assert!(matches!(t.ask(&t.action_args(&a.id)),Asked::Started {action:Some(facts),..} if facts==expected));
    assert!(matches!(t.done(),Collected::Done {action:Some(facts),..} if facts==expected));
    assert_eq!(t.lua("for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='workspace/executeCommand' then return true end end return false",vec![]),false.into());
}
#[test]
#[ignore]
fn an_action_past_ten_minutes_is_gone() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Old'}");
    let left = t.lua(
        "local id=... local s,u=vim.uv.gettimeofday() return _G.__eitri_agent.lsp.actions['test:'..id].deadline_ms - (s*1000+u/1000)",
        vec![a.id.clone().into()],
    );
    let left = left.as_f64().expect("a number");
    assert!((599_000.0..=600_000.0).contains(&left), "{left}");
    t.lua(
        "local id=... _G.__eitri_agent.lsp.actions['test:'..id].deadline_ms=0",
        vec![a.id.clone().into()],
    );
    assert_eq!(t.ask(&t.action_args(&a.id)), Asked::ActionGone);
}
#[test]
#[ignore]
fn an_action_of_a_stopped_client_is_gone() {
    let mut t = Test::new();
    let a = t.list_action("return {title='Stopped'}");
    t.lua("for _,c in ipairs(vim.lsp.get_clients()) do c:stop(true) end", vec![]);
    t.rpc.0.borrow_mut().wait_until("client stopped", |n| {
        n.lua("return #vim.lsp.get_clients()==0", vec![]).as_bool() == Some(true)
    });
    assert_eq!(t.ask(&t.action_args(&a.id)), Asked::ActionGone);
}
#[test]
#[ignore]
fn an_action_on_a_buffer_edited_and_saved_since_is_gone() {
    let mut t = Test::new();
    let a = t.list_action("local e=... return {title='Stale',edit=e}");
    t.lua("local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'changed'}) vim.api.nvim_buf_call(b,function() vim.cmd.write() end)",vec![t.buf.clone()]);
    assert_eq!(t.ask(&t.action_args(&a.id)), Asked::ActionGone);
}
#[test]
#[ignore]
fn another_buffers_edit_leaves_an_action_alone() {
    let mut t = Test::new();
    let a = t.list_action("local e=... return {title='Still valid',edit=e}");
    t.lua(
        "local b=vim.api.nvim_create_buf(true,false) vim.api.nvim_buf_set_lines(b,0,-1,true,{'other'})",
        vec![],
    );
    assert!(matches!(t.ask(&t.action_args(&a.id)), Asked::Started { .. }));
    t.edit_done();
}
#[test]
#[ignore]
fn a_rename_answers_the_ticks_of_the_ask() {
    let mut t = Test::new();
    t.edit_server();
    let other = t.lua(
        OPEN,
        vec![{
            let p = t.root.join("b.rs");
            std::fs::write(&p, "other\n").unwrap();
            p.to_string_lossy().as_ref().into()
        }],
    );
    let tick = t
        .lua(
            "local b=... return vim.api.nvim_buf_get_changedtick(b)",
            vec![other.clone()],
        )
        .as_u64()
        .unwrap();
    t.answers(map(&[("textDocument/rename", t.workspace_edit(false))]), &[], 30);
    let before = wall_ms();
    t.ask(&t.edit_args(LspKind::Rename));
    let after = wall_ms();
    t.lua(
        "local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'moved'})",
        vec![other.clone()],
    );
    match t.done() {
        Collected::Done { ticks, asked_ms, .. } => {
            assert_eq!(ticks.get(&other.as_u64().unwrap()), Some(&tick));
            assert!(ticks.contains_key(&t.buf.as_u64().unwrap()));
            let asked = asked_ms.expect("the time of the ask");
            assert!(before <= asked && asked <= after + 1, "{before} {asked} {after}");
        }
        v => panic!("{v:?}"),
    }
}
#[test]
#[ignore]
fn targets_say_found_and_modified_and_add_no_buffer() {
    let mut t = Test::new();
    t.lua(
        "local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'unsaved'})",
        vec![t.buf.clone()],
    );
    let before = t.lua("return #vim.api.nvim_list_bufs()", vec![]);
    let stamp = t.seq.stamp(EDITOR_BUDGET);
    let value = lsp::targets(&*t.rpc, &stamp, &[t.path.clone(), t.root.join("missing")])
        .try_take()
        .unwrap()
        .unwrap();
    let states = lsp::parse_targets(&value).unwrap();
    assert_eq!(states.len(), 2);
    assert!(states[0].found && states[0].modified);
    assert_eq!(states[0].buf, t.buf.as_u64());
    assert!(states[0].changedtick.is_some());
    assert!(!states[1].found);
    assert_eq!(states[1].buf, None);
    assert_eq!(before, t.lua("return #vim.api.nvim_list_bufs()", vec![]));
    let paths = vec![t.path.clone(); 65];
    let stamp = t.seq.stamp(EDITOR_BUDGET);
    assert!(lsp::targets(&*t.rpc, &stamp, &paths).try_take().unwrap().is_err());
}
#[test]
#[ignore]
fn a_target_hash_equals_the_hash_rust_computes() {
    for bytes in [b"a\nb\n".as_slice(), b"a\r\nb\r\n", b"a\nb"] {
        let mut t = Test::new();
        std::fs::write(&t.path, bytes).unwrap();
        t.lua(
            "local b=... vim.api.nvim_buf_call(b,function() vim.cmd('edit!') end)",
            vec![t.buf.clone()],
        );
        let stamp = t.seq.stamp(EDITOR_BUDGET);
        let value = lsp::targets(&*t.rpc, &stamp, &[t.path.clone()])
            .try_take()
            .unwrap()
            .unwrap();
        let state = lsp::parse_targets(&value).unwrap().remove(0);
        assert_eq!(
            state.sha256,
            eitri_core::editor_tools::text_edits::buffer_digest(bytes, state.format)
        );
    }
}
#[test]
#[ignore]
fn a_target_says_how_its_file_ends() {
    use eitri_core::editor_tools::text_edits::final_newline_agrees;
    for (bytes, eol) in [
        (b"a\nb\n".as_slice(), true),
        (b"a\r\nb\r\n", true),
        (b"a\nb", false),
        (b"a\r\nb", false),
        (b"", true),
        (b"\n", true),
    ] {
        let mut t = Test::new();
        std::fs::write(&t.path, bytes).unwrap();
        t.lua(
            "local b=... vim.api.nvim_buf_call(b,function() vim.cmd('edit!') end)",
            vec![t.buf.clone()],
        );
        let stamp = t.seq.stamp(EDITOR_BUDGET);
        let value = lsp::targets(&*t.rpc, &stamp, &[t.path.clone()])
            .try_take()
            .unwrap()
            .unwrap();
        let state = lsp::parse_targets(&value).unwrap().remove(0);
        assert_eq!(state.eol, eol, "{bytes:?}");
        assert!(final_newline_agrees(bytes, state.eol), "{bytes:?}");
    }
}
#[test]
#[ignore]
fn a_file_the_editor_saved_with_a_new_final_newline_disagrees_until_it_is_reloaded() {
    use eitri_core::editor_tools::text_edits::final_newline_agrees;
    let mut t = Test::new();
    std::fs::write(&t.path, b"a").unwrap();
    let state_after = |t: &mut Test, command: &str| {
        t.lua(
            "local b,c=... vim.api.nvim_buf_call(b,function() vim.cmd(c) end)",
            vec![t.buf.clone(), command.into()],
        );
        let stamp = t.seq.stamp(EDITOR_BUDGET);
        let value = lsp::targets(&*t.rpc, &stamp, std::slice::from_ref(&t.path))
            .try_take()
            .unwrap()
            .unwrap();
        lsp::parse_targets(&value).unwrap().remove(0)
    };
    assert!(!state_after(&mut t, "edit!").eol);
    let saved = state_after(&mut t, "write");
    let disk = std::fs::read(&t.path).unwrap();
    assert_eq!(disk, b"a\n");
    assert!(!saved.modified);
    assert!(!final_newline_agrees(&disk, saved.eol));
    let reloaded = state_after(&mut t, "edit");
    assert!(final_newline_agrees(&disk, reloaded.eol));
}
#[test]
#[ignore]
fn a_title_past_its_cap_stays_whole_characters() {
    let mut t = Test::new();
    let a = t.list_action(
        "return {title=string.rep('é',600),kind=string.rep('𝒳',300),disabled={reason=string.rep('é',600)}}",
    );
    assert_eq!(a.title, "é".repeat(512));
    assert_eq!(a.kind, Some("𝒳".repeat(256)));
    assert_eq!(a.disabled, Some("é".repeat(512)));
}

#[test]
#[ignore]
fn code_action_limits_keep_sixty_four_and_evict_the_oldest_after_two_hundred_fifty_six() {
    let mut t = Test::new();
    t.edit_server();
    let actions = t.lua(
        "local out={} for i=1,70 do out[i]={title='Fix '..i} end return out",
        vec![],
    );
    let mut first = None;
    let mut latest = None;
    for i in 0..5 {
        t.answers(map(&[("textDocument/codeAction", actions.clone())]), &[], 0);
        let mut args = t.edit_args(LspKind::CodeActions);
        args.id = format!("list-{i}");
        assert!(matches!(t.ask(&args), Asked::Started { .. }));
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            match t.collect(&args.id, false) {
                Collected::Pending => {
                    assert!(Instant::now() < end, "the action list did not arrive");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Collected::Done { actions, .. } => {
                    assert_eq!(actions.len(), 64);
                    first.get_or_insert_with(|| actions[0].id.clone());
                    latest = Some(actions[0].id.clone());
                    break;
                }
                v => panic!("{v:?}"),
            }
        }
    }
    assert_eq!(
        t.lua(
            "local n=0 for _ in pairs(_G.__eitri_agent.lsp.actions) do n=n+1 end return n",
            vec![]
        ),
        256.into()
    );
    assert_eq!(t.ask(&t.action_args(first.as_deref().unwrap())), Asked::ActionGone);
    t.lua("for _,c in ipairs(vim.lsp.get_clients()) do c.server_capabilities.codeActionProvider={resolveProvider=false} end", vec![]);
    assert_eq!(
        t.ask(&t.action_args(latest.as_deref().unwrap())),
        Asked::NoEdit { has_command: false }
    );
}

#[test]
#[ignore]
fn an_action_keeps_the_ticks_from_its_list_when_resolved_later() {
    let mut t = Test::new();
    let other_path = t.root.join("b.rs");
    std::fs::write(&other_path, "old\n").unwrap();
    let other = t.lua(OPEN, vec![other_path.to_string_lossy().as_ref().into()]);
    let tick = t
        .lua(
            "local b=... return vim.api.nvim_buf_get_changedtick(b)",
            vec![other.clone()],
        )
        .as_u64()
        .unwrap();
    let action = t.list_action("local e=... return {title='Fix',edit=e}");
    t.lua(
        "local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'new'})",
        vec![other.clone()],
    );
    assert!(matches!(t.ask(&t.action_args(&action.id)), Asked::Started { .. }));
    match t.done() {
        Collected::Done { ticks, .. } => assert_eq!(ticks.get(&other.as_u64().unwrap()), Some(&tick)),
        v => panic!("{v:?}"),
    }
}

fn action_tool(
    t: &mut Test,
    memory: &eitri_core::editor_tools::memory::Memory,
    name: &str,
    arguments: serde_json::Value,
) -> ToolResult {
    use eitri_core::editor_tools::step::{AgentCall, EditorReach, EditorSight, Step, StepEvent, ToolCx};
    let cx = ToolCx {
        mode: PermissionMode::Manual,
        trust: ProjectTrust::Trusted,
        root: &t.root,
        home: None,
        reach: EditorReach::Own,
        sight: EditorSight::OnScreen,
        context: None,
        tab: 1,
        session: "session-1",
        call: "action-tool",
        agent_call: AgentCall::Host,
        memory,
    };
    let tool = eitri_core::editor_tools::tools::tool_named(name).expect("the action tool is registered");
    let mut run = tool.begin(&arguments, &cx).unwrap();
    let way = LocalWay(t.rpc.clone());
    let end = Instant::now() + LSP_BUDGET * 2;
    let mut event = StepEvent::Start;
    loop {
        assert!(Instant::now() < end, "the action tool did not finish");
        event = match run.step(event, &cx) {
            Step::Done(result) => return result,
            Step::Call { call, budget } => {
                let stamp = t.seq.stamp(budget);
                let reply = way.call(cx.session, &call, &stamp).try_take().unwrap().unwrap();
                StepEvent::Answer(Ok(reply))
            }
            Step::PollAfter(delay) => {
                std::thread::sleep(delay);
                StepEvent::Poll
            }
            Step::Ask(card) => panic!("a loaded action needs no card: {card:?}"),
        };
    }
}

fn action_list(t: &mut Test, memory: &eitri_core::editor_tools::memory::Memory) -> String {
    let result = action_tool(t, memory, "lsp_code_actions", json!({"path":"a.rs", "line":1}));
    assert!(!result.is_error, "{}", result.text);
    let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(answer["actions"][0]["applies"], true);
    answer["actions"][0]["id"].as_str().unwrap().to_owned()
}

#[test]
#[ignore]
fn a_listed_action_resolves_to_an_edit_end_to_end() {
    let mut t = Test::new();
    t.edit_server();
    let actions = t.lua("return {{title='Resolve <fix>',data={token=42}}}", vec![]);
    t.answers(map(&[("textDocument/codeAction", actions)]), &[], 0);
    let memory = eitri_core::editor_tools::memory::Memory::default();
    memory.set_writer(Some(eitri_core::turn_review::ProposalWriter::new(
        Some(t.dir.join("review")),
        &t.root,
    )));
    let state = t.lua(
        "return {vim.api.nvim_get_current_win(),vim.api.nvim_win_get_cursor(0),vim.api.nvim_get_mode().mode}",
        vec![],
    );
    let id = action_list(&mut t, &memory);
    assert!(memory.proposal(1, "p-1").is_none());
    let resolved = t.lua(
        "local e=... return {title='Resolved <fix>',edit=e,command={command='server.run'}}",
        vec![t.workspace_edit(true)],
    );
    t.answers(map(&[("codeAction/resolve", resolved)]), &[], 0);
    let result = action_tool(&mut t, &memory, "apply_code_action", json!({"id":id}));
    assert!(!result.is_error, "{}", result.text);
    let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(answer["command_not_run"], true);
    assert_eq!(answer["files"][0]["path"], "a.rs");
    let proposal = memory.proposal(1, answer["proposal"].as_str().unwrap()).unwrap();
    assert_eq!(proposal.what, "the code action \"Resolve <fix>\"");
    let (before, after) = memory.writer().unwrap().texts(&proposal.files[0]).unwrap();
    assert_eq!(before, "foo\n");
    assert_eq!(after, "bar\n");
    assert_eq!(std::fs::read(&t.path).unwrap(), b"foo\n");
    assert_eq!(
        t.lua(
            "local b=... return vim.api.nvim_buf_get_lines(b,0,-1,true)",
            vec![t.buf.clone()]
        ),
        Value::Array(vec!["foo".into()])
    );
    assert_eq!(
        state,
        t.lua(
            "return {vim.api.nvim_get_current_win(),vim.api.nvim_win_get_cursor(0),vim.api.nvim_get_mode().mode}",
            vec![]
        )
    );
    assert_eq!(t.lua("local n=0 for _,r in ipairs(_G.__fake_lsp.seen) do assert(r.method~='workspace/executeCommand') if r.method=='codeAction/resolve' then assert(r.params.data.token==42) n=n+1 end end return n",vec![]), 1.into());
}

#[test]
#[ignore]
fn an_action_listed_before_an_edit_and_save_is_refused_end_to_end() {
    let mut t = Test::new();
    t.edit_server();
    let actions = t.lua(
        "local e=... return {{title='Stale fix',edit=e}}",
        vec![t.workspace_edit(false)],
    );
    t.answers(map(&[("textDocument/codeAction", actions)]), &[], 0);
    let memory = eitri_core::editor_tools::memory::Memory::default();
    memory.set_writer(Some(eitri_core::turn_review::ProposalWriter::new(
        Some(t.dir.join("review")),
        &t.root,
    )));
    let id = action_list(&mut t, &memory);
    t.lua("local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'changed'}) vim.api.nvim_buf_call(b,function() vim.cmd.write() end)", vec![t.buf.clone()]);
    let result = action_tool(&mut t, &memory, "apply_code_action", json!({"id":id}));
    assert!(result.is_error);
    assert_eq!(
        result.text,
        "that code action is no longer known; ask `lsp_code_actions` again"
    );
    assert!(memory.proposal(1, "p-1").is_none());
    assert_eq!(std::fs::read(&t.path).unwrap(), b"changed\n");
    assert_eq!(
        t.lua("local b=... return vim.bo[b].modified", vec![t.buf.clone()]),
        false.into()
    );
    assert_eq!(t.lua("for _,r in ipairs(_G.__fake_lsp.seen) do if r.method=='workspace/executeCommand' then return true end end return false",vec![]), false.into());
}

/// An action listed while `b.rs` is not open, its edit to `b.rs` over `foo`, and a store to keep a proposal in.
fn action_on_a_file_not_open(t: &mut Test) -> (eitri_core::editor_tools::memory::Memory, PathBuf, String) {
    let other = t.root.join("b.rs");
    std::fs::write(&other, "foo();\n").unwrap();
    t.edit_server();
    let actions = t.lua(
        "local p=... return {{title='Rename',edit={changes={[vim.uri_from_fname(p)]={{range={start={line=0,character=0},['end']={line=0,character=3}},newText='bar'}}}}}}",
        vec![other.to_string_lossy().as_ref().into()],
    );
    t.answers(map(&[("textDocument/codeAction", actions)]), &[], 0);
    let memory = eitri_core::editor_tools::memory::Memory::default();
    memory.set_writer(Some(eitri_core::turn_review::ProposalWriter::new(
        Some(t.dir.join("review")),
        &t.root,
    )));
    let id = action_list(t, &memory);
    // A file's time comes from a clock that may lag the one the ask read by a kernel tick.
    std::thread::sleep(Duration::from_millis(20));
    (memory, other, id)
}

const CHANGED_SINCE: &str = "b.rs changed since the language server answered; ask again";

#[test]
#[ignore]
fn a_file_opened_and_saved_after_its_action_was_listed_is_refused() {
    let mut t = Test::new();
    let (memory, other, id) = action_on_a_file_not_open(&mut t);
    let buf = t.lua(OPEN, vec![other.to_string_lossy().as_ref().into()]);
    t.lua(
        "local b=... vim.api.nvim_buf_set_lines(b,0,-1,true,{'// foo();'}) vim.api.nvim_buf_call(b,function() vim.cmd('silent write') end)",
        vec![buf],
    );
    assert_eq!(std::fs::read(&other).unwrap(), b"// foo();\n");

    let result = action_tool(&mut t, &memory, "apply_code_action", json!({"id":id}));
    assert!(result.is_error, "{}", result.text);
    assert_eq!(result.text, CHANGED_SINCE);
    assert!(memory.proposal(1, "p-1").is_none());
}

#[test]
#[ignore]
fn a_file_changed_on_disk_after_its_action_was_listed_is_refused() {
    let mut t = Test::new();
    let (memory, other, id) = action_on_a_file_not_open(&mut t);
    std::fs::write(&other, "// foo();\n").unwrap();

    let result = action_tool(&mut t, &memory, "apply_code_action", json!({"id":id}));
    assert!(result.is_error, "{}", result.text);
    assert_eq!(result.text, CHANGED_SINCE);
}

#[test]
#[ignore]
fn a_file_left_alone_after_its_action_was_listed_is_proposed_with_its_title() {
    let mut t = Test::new();
    let (memory, _other, id) = action_on_a_file_not_open(&mut t);

    let result = action_tool(&mut t, &memory, "apply_code_action", json!({"id":id}));
    assert!(!result.is_error, "{}", result.text);
    let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(answer["title"], "Rename");
    assert_eq!(answer["files"][0]["path"], "b.rs");
}
