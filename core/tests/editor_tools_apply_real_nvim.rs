//! A language server's edit from the ask to the disk and back into the buffer: an embedded nvim with Eitri's reload
//! timer, a real project and store, the real broker, an in-process language server. No sockets and no display.
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
use eitri_core::buffer_reload::RELOAD_CMD;
use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_context::EditorContext;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
use eitri_core::editor_tools::cards::{CardDecision, CardLine, CardOrigin, EditorCardWire};
use eitri_core::editor_tools::step::ToolResult;
use eitri_core::editor_tools::tools::lsp::LSP_BUDGET;
use eitri_core::editor_tools::way::{EditorWay, LocalWay};
use eitri_core::nvim_rpc::Pending;
use eitri_core::tabs::TabId;
use eitri_core::turn_review::ProposalWriter;
use embed_client::{opts_map as map, Embed};
use rmpv::Value;
use serde_json::json;

const FAKE: &str = include_str!("support/fake_lsp.lua");
const OPEN: &str = "local p = ... local b = vim.fn.bufadd(p) vim.fn.bufload(b) return b";
const SET: &str = "local answers = ... _G.__fake_lsp = {seen = {}, answers = answers, hang = {}, delay = 0}";
const RENAME: &str = "local a, b = ... local e = {{range={start={line=0,character=0}, ['end']={line=0,character=3}}, \
                      newText='bar'}} return {changes={[vim.uri_from_fname(a)]=e,[vim.uri_from_fname(b)]=e}}";
const SHOW: &str = "local b = ... vim.api.nvim_set_current_buf(b)";
const LINES: &str = "local b = ... return vim.api.nvim_buf_get_lines(b, 0, -1, true)";
const MODIFIED: &str = "local b = ... return vim.bo[b].modified";
const EDIT: &str = "local b = ... vim.api.nvim_buf_set_lines(b, 0, -1, true, {'foo typed'})";
const LOADED: &str = "local p = ... return vim.fn.bufloaded(p)";
const ATTACHED: &str = "local b = ... return #vim.lsp.get_clients({bufnr=b}) > 0";

const TAB: TabId = TabId(1);
/// One tick of the reload timer and room to run it.
const RELOADED_WITHIN: Duration = Duration::from_secs(2);

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

/// One Manual tab of a window whose editor is the test's nvim.
struct Env {
    way: LocalWay,
    root: PathBuf,
    writer: ProposalWriter,
    declared: Rc<BTreeSet<String>>,
}

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

    fn proposal_writer(&self) -> Option<ProposalWriter> {
        Some(self.writer.clone())
    }
}

struct Test {
    rpc: Rc<Rpc>,
    feed: EditorContextFeed,
    env: Env,
    broker: Broker,
    dir: PathBuf,
    a: PathBuf,
    b: PathBuf,
    buf: Value,
    calls: usize,
}

impl Test {
    /// [`Test::loaded`], the first file in the window.
    fn new() -> Test {
        let t = Test::loaded();
        t.show();
        t
    }

    /// Two project files, the first loaded in no window, with a language server that answers a rename in both.
    fn loaded() -> Test {
        agent::state_dirs::redirect_state_to_a_test_root();
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "apply-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let root = dir.join("project");
        let (a, b) = (root.join("a.rs"), root.join("b.rs"));
        std::fs::write(&a, "foo\n").unwrap();
        std::fs::write(&b, "foo\n").unwrap();

        let feed = EditorContextFeed::new().expect("the feed must bind");
        let manual = format!("lua {}", include_str!("support/manual_timers.lua"));
        let mut nvim = Embed::start_fed(&dir, &feed, &["--cmd", &manual, "--cmd", RELOAD_CMD]);
        let path = |file: &Path| Value::from(file.to_string_lossy().as_ref());
        let buf = nvim.lua(OPEN, vec![path(&a)]);
        let config = map(&[
            ("name", "edits".into()),
            ("encoding", "utf-16".into()),
            ("root", path(&root)),
            ("capabilities", map(&[("renameProvider", true.into())])),
        ]);
        nvim.lua(FAKE, vec![buf.clone(), config]);
        nvim.wait_until("the client to attach", |nvim| {
            nvim.lua(ATTACHED, vec![buf.clone()]).as_bool() == Some(true)
        });
        let edit = nvim.lua(RENAME, vec![path(&a), path(&b)]);
        nvim.lua(SET, vec![map(&[("textDocument/rename", edit)])]);

        let rpc = Rc::new(Rpc(RefCell::new(nvim)));
        let declared = ["lsp_rename", "apply_proposal"].map(str::to_owned);
        let env = Env {
            way: LocalWay(rpc.clone()),
            writer: ProposalWriter::new(Some(dir.join("review")), &root),
            root,
            declared: Rc::new(BTreeSet::from(declared)),
        };
        Test {
            rpc,
            feed,
            env,
            broker: Broker::new(),
            dir,
            a,
            b,
            buf,
            calls: 0,
        }
    }

    fn lua(&self, code: &str, args: Vec<Value>) -> Value {
        self.rpc.0.borrow_mut().lua(code, args)
    }

    fn submit(&mut self, tool: &str, arguments: &serde_json::Value) {
        self.calls += 1;
        let caller = Caller::Sidecar {
            call_id: format!("call-{}", self.calls),
        };
        let text = arguments.to_string();
        let out = self
            .broker
            .submit(TAB, caller, None, tool, &text, &self.env, Instant::now());
        assert!(out.is_empty(), "{out:?}");
    }

    fn answered(&mut self) -> ToolResult {
        let end = Instant::now() + LSP_BUDGET * 2;
        loop {
            for out in self.broker.tick(&self.env, Instant::now()) {
                if let BrokerOut::Answer { result, .. } = out {
                    return result;
                }
            }
            assert!(Instant::now() < end, "the agent was not answered");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn card(&mut self) -> EditorCardWire {
        let end = Instant::now() + LSP_BUDGET * 2;
        loop {
            let out = self.broker.tick(&self.env, Instant::now());
            assert!(
                !out.iter().any(|out| matches!(out, BrokerOut::Answer { .. })),
                "{out:?}"
            );
            if let Some(card) = self.broker.cards(TAB).pop() {
                return card;
            }
            assert!(Instant::now() < end, "no card was raised");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn allow(&mut self, card: &EditorCardWire) {
        self.broker
            .answer_card(TAB, &card.id, CardDecision::Allow, CardOrigin::Key, Instant::now())
            .unwrap();
    }

    /// The server's rename as a proposal, and its id. Nothing has changed yet.
    fn proposal(&mut self) -> String {
        self.submit(
            "lsp_rename",
            &json!({"path": "a.rs", "line": 1, "symbol": "foo", "new_name": "bar"}),
        );
        let result = self.answered();
        assert!(!result.is_error, "{}", result.text);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(std::fs::read(&self.a).unwrap(), b"foo\n");
        assert_eq!(std::fs::read(&self.b).unwrap(), b"foo\n");
        answer["proposal"].as_str().unwrap().to_owned()
    }

    fn show(&self) {
        self.lua(SHOW, vec![self.buf.clone()]);
    }

    fn lines(&self) -> Value {
        self.lua(LINES, vec![self.buf.clone()])
    }
}

impl Drop for Test {
    fn drop(&mut self) {
        self.feed.cleanup();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_rename_through_a_fake_server_reaches_the_disk_and_the_buffer_reloads() {
    let mut t = Test::new();
    let id = t.proposal();
    t.submit("apply_proposal", &json!({ "id": id }));

    let card = t.card();
    assert_eq!(card.tool, "apply_proposal");
    assert_eq!(card.title, "Apply rename foo to bar to 2 files?");
    let proposal = card.proposal.clone().expect("the card names its proposal");
    assert_eq!(proposal.id, id);
    assert_eq!(
        BTreeSet::from_iter(proposal.files),
        BTreeSet::from(["a.rs".to_owned(), "b.rs".to_owned()])
    );
    assert!(card
        .body
        .iter()
        .any(|line| matches!(line, CardLine::Code(text) if text.contains("-foo\n+bar"))));
    assert_eq!(std::fs::read(&t.a).unwrap(), b"foo\n", "a card writes nothing");

    t.allow(&card);
    let result = t.answered();
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(std::fs::read(&t.a).unwrap(), b"bar\n");
    assert_eq!(std::fs::read(&t.b).unwrap(), b"bar\n");

    let end = Instant::now() + RELOADED_WITHIN;
    while t.lines() != Value::Array(vec!["bar".into()]) {
        assert!(Instant::now() < end, "the buffer did not reload: {:?}", t.lines());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(t.lua(MODIFIED, vec![t.buf.clone()]), false.into());
    let b = t.b.to_string_lossy().into_owned();
    assert_eq!(
        t.lua(LOADED, vec![b.as_str().into()]),
        0.into(),
        "a file not open stays so"
    );

    t.submit("apply_proposal", &json!({ "id": id }));
    let again = t.answered();
    assert!(again.is_error);
    assert!(again.text.starts_with("no proposal"), "{}", again.text);
}

/// `checktime` looks only at buffers in a window, so a buffer in none stays as it was until it is shown, and showing it
/// is what reloads it. A person never sees the old text; a proposal asked before then is refused by its digest.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_in_no_window_catches_up_when_it_is_shown() {
    let mut t = Test::loaded();
    let id = t.proposal();
    t.submit("apply_proposal", &json!({ "id": id }));
    let card = t.card();
    t.allow(&card);
    let result = t.answered();
    assert!(!result.is_error, "{}", result.text);
    t.rpc.0.borrow_mut().advance_timers(RELOADED_WITHIN.as_millis() as u64);
    assert_eq!(t.lines(), Value::Array(vec!["foo".into()]), "two ticks passed it by");
    t.show();
    assert_eq!(t.lines(), Value::Array(vec!["bar".into()]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_buffer_edited_after_the_proposal_blocks_the_write() {
    let mut t = Test::new();
    let id = t.proposal();
    t.submit("apply_proposal", &json!({ "id": id }));
    let card = t.card();

    t.lua(EDIT, vec![t.buf.clone()]);
    t.allow(&card);
    let result = t.answered();
    assert!(result.is_error);
    assert_eq!(result.text, "a.rs has unsaved changes in your editor");
    assert_eq!(std::fs::read(&t.a).unwrap(), b"foo\n");
    assert_eq!(std::fs::read(&t.b).unwrap(), b"foo\n");
    assert_eq!(t.lines(), Value::Array(vec!["foo typed".into()]));
    assert_eq!(t.lua(MODIFIED, vec![t.buf.clone()]), true.into());
}
