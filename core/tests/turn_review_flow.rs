//! The review flow against a real tab set, real git and real files, with a fake editor: every
//! request is answered once, nothing is written for a tab, a session or an editor that is no longer
//! the one asked, and nothing waits on the calling thread.
//!
//! The recovery tests re-run this binary as a child whose in-place write dies halfway. The child
//! test is `#[ignore]`d and does nothing unless the parent set its directory variable, so
//! `-- --ignored` over this target is harmless.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::{AgentConversation, AgentDomainEvent, TurnOutcome};
use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
use eitri_core::agent_bridge::{parse_inbound_message, InboundMessage, SessionModeChoice};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::review_editor::{
    editor_event_to_wire, parse_revert_event, EventKind, Owner, RemoteOverlay, CLEAR_LUA, LIVE_CLEAR_LUA,
    LIVE_SHOWN_LUA, LIVE_SHOW_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, SHOW_LUA, TAKE_EVENTS_LUA,
};
use eitri_core::tab_set::{TabBackend, TabSet};
use eitri_core::tabs::TabId;
use eitri_core::test_providers::RecordingProvider;
use eitri_core::turn_review::{
    check_reverts, replace, review_dir_for, saved_to_undo, show_hunk, undo_to_saved, Blocked, Busy, Content, FsHooks,
    Journal, JournalNote, NewRevert, Out, PresenceGuard, PresenceHolder, ProjectDir, RemoteAsk, RevertShape,
    RevertSource, RevertStatus, ReviewFlow, ReviewOptions, Saved, Scope, Shadow, SlotBusy, Snap, Stage, TurnReview,
    UndoState, WriteSlot,
};
use eitri_core::window_link::{Drawn, EditorOp};
use rmpv::Value;
use serde_json::{json, Value as Json};

const SESSION: &str = "flow-session";
const CHILD_DIR: &str = "EITRI_FLOW_CHILD_DIR";
const CHILD_REL: &str = "EITRI_FLOW_CHILD_REL";
/// A pid no real window here has.
const OTHER_PID: u32 = 999_999;

// ---- scratch and files -----------------------------------------------------------------------------

/// A directory of the test's own under the system's temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        // The system's temporary directory, not the target dir: these tests need user extended
        // attributes (the in-place write), git's fsmonitor and a disk fast enough for the snapshot
        // budget, and a checkout on a network mount has none of the three. A fresh directory of
        // this user's own holds it: a random name made here at 0700, never one already in place in
        // a shared directory, set exactly so whatever the umask, and removed on drop.
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "eitri-turn_review_flow-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A snapshot or journal worker a test started can still write here for a moment after the
        // test's last line (they are not joined), and a removal racing it leaves part of the tree,
        // or the worker makes it again. So the directory must stay gone for a while, up to 5 s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut absent_since = None;
        while std::time::Instant::now() < deadline {
            if self.0.exists() {
                let _ = std::fs::remove_dir_all(&self.0);
                absent_since = None;
            } else if absent_since.get_or_insert_with(std::time::Instant::now).elapsed()
                > std::time::Duration::from_millis(100)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let _ = std::fs::remove_file(path);
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// The bytes, inode and modification time of a file, none of which a refused write may change.
fn fingerprint(path: &Path) -> (Vec<u8>, u64, i64, i64) {
    let m = std::fs::symlink_metadata(path).unwrap();
    (std::fs::read(path).unwrap(), m.ino(), m.mtime(), m.mtime_nsec())
}

fn tgit(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// ---- the editor ------------------------------------------------------------------------------------

/// An editor that answers every question with `answer`, or holds each one until [`release`](Self::release):
/// a held question stays alive, because a dropped one would read as a closed connection.
struct FakeEditor {
    answer: RefCell<Option<Value>>,
    target: Cell<Option<u64>>,
    asked: RefCell<Vec<(&'static str, Vec<Value>)>>,
    held: RefCell<Vec<Answer>>,
}

impl FakeEditor {
    /// Holds every answer.
    fn holding() -> FakeEditor {
        FakeEditor {
            answer: RefCell::new(None),
            target: Cell::new(Some(7)),
            asked: RefCell::new(Vec::new()),
            held: RefCell::new(Vec::new()),
        }
    }

    fn answering(value: Value) -> FakeEditor {
        let fake = FakeEditor::holding();
        *fake.answer.borrow_mut() = Some(value);
        fake
    }

    /// Answers every held question with `value`.
    fn release(&self, value: Value) {
        for answer in self.held.borrow_mut().drain(..) {
            answer.send(Ok(value.clone()));
        }
    }

    fn rpc(&self) -> Option<&dyn EditorRpc> {
        Some(self)
    }

    fn asked(&self) -> usize {
        self.asked.borrow().len()
    }
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        self.asked.borrow_mut().push((code, args));
        let (answer, pending) = Pending::pair();
        match self.answer.borrow().as_ref() {
            Some(value) => answer.send(Ok(value.clone())),
            None => self.held.borrow_mut().push(answer),
        }
        pending
    }

    fn target(&self) -> Option<u64> {
        self.target.get()
    }
}

/// What the editor answers when no buffer shows the file and nothing is unsaved.
fn clear_buffer() -> Value {
    Value::Map(vec![
        (Value::from("found"), Value::from(false)),
        (Value::from("modified"), Value::from(false)),
    ])
}

fn modified_buffer() -> Value {
    Value::Map(vec![
        (Value::from("found"), Value::from(true)),
        (Value::from("modified"), Value::from(true)),
    ])
}

/// A buffer of `lines` (no terminators) in `fileformat`, unmodified or not.
fn buffer_with(lines: &[&str], fileformat: &str, eol: bool, modified: bool) -> Value {
    Value::Map(vec![
        (Value::from("found"), Value::from(true)),
        (Value::from("modified"), Value::from(modified)),
        (Value::from("fileformat"), Value::from(fileformat)),
        (Value::from("eol"), Value::from(eol)),
        (Value::from("line_count"), Value::from(lines.len() as u64)),
        (
            Value::from("lines"),
            Value::Array(lines.iter().map(|l| Value::from(*l)).collect()),
        ),
    ])
}

// ---- messages and answers --------------------------------------------------------------------------

fn msg(v: Json) -> InboundMessage {
    parse_inbound_message(&v.to_string()).expect("the message parses")
}

fn revert_file(id: &str, tab: TabId, turn: u32, path: &str) -> InboundMessage {
    msg(
        json!({"type":"review_revert","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,"target":"file"}),
    )
}

fn revert_hunk(id: &str, tab: TabId, turn: u32, path: &str, hunk: u32, header: &str) -> InboundMessage {
    msg(
        json!({"type":"review_revert","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,
        "target":{"hunk":hunk,"header":header}}),
    )
}

fn undo(id: &str, tab: TabId) -> InboundMessage {
    msg(json!({"type":"review_undo","request_id":id,"tab":tab.0}))
}

fn comment(id: &str, tab: TabId, turn: u32, path: &str, text: &str) -> InboundMessage {
    msg(
        json!({"type":"review_comment_add","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,
        "from":1,"to":1,"text":text}),
    )
}

fn send(id: &str, tab: TabId, confirm: Option<&str>) -> InboundMessage {
    msg(json!({"type":"review_send","request_id":id,"tab":tab.0,"confirm":confirm}))
}

fn recover(id: &str, entry: &str, answer: &str) -> InboundMessage {
    msg(json!({"type":"review_recover","request_id":id,"entry":entry,"answer":answer}))
}

/// The text of `out` if it is an envelope. An answer names the request its envelope carries, and a
/// draft on its own the tab it is about, so the panel can route each to the page it belongs to.
fn envelope_text(out: &Out) -> Option<&str> {
    match out {
        Out::Envelope(text) => Some(text),
        Out::Answer { request_id, payload } => {
            let v: Json = serde_json::from_str(payload).expect("an envelope is JSON");
            assert_eq!(v["requestId"], *request_id, "an answer names its own request");
            Some(payload)
        }
        Out::Draft(tab, payload) => {
            let v: Json = serde_json::from_str(payload).expect("an envelope is JSON");
            assert_eq!(v["tab"], tab.0, "a draft names its own tab");
            assert!(v["requestId"].is_null(), "a draft that answers a request is an answer");
            Some(payload)
        }
        Out::Flushed(..) | Out::QueueChanged(..) | Out::EditorOpened => None,
    }
}

/// The envelopes among `outs`, parsed.
fn envelopes(outs: &[Out]) -> Vec<Json> {
    outs.iter()
        .filter_map(envelope_text)
        .map(|text| serde_json::from_str(text).expect("an envelope is JSON"))
        .collect()
}

/// One word per out, in order: `kind`, `kind:requestId` and `ok`/`failed` for a command result,
/// `flushed` for a send that went out.
fn shape(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .map(|out| match out {
            Out::Flushed(..) => "flushed".to_string(),
            Out::QueueChanged(..) => "queue_changed".to_string(),
            Out::EditorOpened => "editor_opened".to_string(),
            out @ (Out::Envelope(_) | Out::Answer { .. } | Out::Draft(..)) => {
                let text = envelope_text(out).expect("an envelope");
                let v: Json = serde_json::from_str(text).unwrap();
                let kind = v["kind"].as_str().unwrap().to_string();
                let id = match &v["requestId"] {
                    Json::String(s) => s.clone(),
                    _ => "null".to_string(),
                };
                if kind == "command_result" {
                    format!("{kind}:{id}:{}", if v["ok"] == true { "ok" } else { "failed" })
                } else {
                    format!("{kind}:{id}")
                }
            }
        })
        .collect()
}

/// The command result for `id` among `outs`: `Ok(message)` or `Err(error)`.
fn result_of(outs: &[Out], id: &str) -> Result<Option<String>, String> {
    let found = envelopes(outs)
        .into_iter()
        .find(|v| v["kind"] == "command_result" && v["requestId"] == id)
        .unwrap_or_else(|| panic!("no command_result for {id} in {:?}", shape(outs)));
    if found["ok"] == true {
        Ok(found["message"].as_str().map(str::to_owned))
    } else {
        Err(found["error"].as_str().unwrap().to_owned())
    }
}

fn answered(outs: &[Out], id: &str) -> bool {
    envelopes(outs).iter().any(|v| v["requestId"] == id)
}

// ---- the world -------------------------------------------------------------------------------------

struct Fixture {
    scratch: Scratch,
    project: PathBuf,
    review_dir: PathBuf,
    tabs: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
    flow: ReviewFlow,
    turns: u32,
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn live_backend(project: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
    let provider = Arc::new(RecordingProvider::default());
    let conversation = AgentConversation::create(
        provider.clone(),
        project,
        agent::setting_sources::ProjectTrust::Untrusted,
    )
    .unwrap();
    (
        provider,
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        },
    )
}

impl Fixture {
    fn settle_file_jobs(&mut self, rpc: Option<&dyn EditorRpc>) -> Vec<Out> {
        let mut out = Vec::new();
        until("file workers to finish", || {
            self.pump();
            out.extend(self.flow.tick(&mut self.tabs, rpc, Instant::now()));
            !self.flow.file_jobs_in_flight_for_tests()
        });
        out
    }

    fn new(name: &str) -> Fixture {
        let mut f = Fixture::bare(name);
        f.give_session();
        f
    }

    /// A tab set with a review and a held presence, and one empty tab.
    fn bare(name: &str) -> Fixture {
        Fixture::bare_with(name, true)
    }

    /// [`Fixture::new`] in a window that holds no presence.
    fn without_presence(name: &str) -> Fixture {
        let mut f = Fixture::bare_with(name, false);
        f.give_session();
        f
    }

    fn bare_with(name: &str, presence: bool) -> Fixture {
        agent::state_dirs::redirect_state_to_a_test_root();
        let scratch = Scratch::new(name);
        let project = scratch.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let review_dir = review_dir_for(Some(scratch.0.join("state").as_os_str()), None, &project).unwrap();
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let options = ReviewOptions {
            excludes: Some(None),
            trim_every: Duration::MAX,
            ..ReviewOptions::default()
        };
        tabs.install_turn_review(TurnReview::with_options(Some(review_dir.clone()), &project, options))
            .unwrap();
        if presence {
            tabs.hold_presence(PresenceHolder::start(review_dir.clone(), std::process::id()));
            let guard = tabs.presence_guard().expect("presence is held");
            until("this window's presence", || guard.check().is_ok());
        }
        let tab = tabs.active();
        Fixture {
            scratch,
            project,
            review_dir,
            tabs,
            tab,
            provider: Arc::new(RecordingProvider::default()),
            flow: ReviewFlow::new(),
            turns: 0,
        }
    }

    fn give_session(&mut self) {
        let (provider, backend) = live_backend(&self.project);
        self.tabs
            .get_mut(self.tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session(SESSION, &self.project);
        self.provider = provider;
        let (tab, project) = (self.tab, self.project.clone());
        until("the session", || {
            self.tabs.pump(&project, true);
            self.tabs.get(tab).unwrap().provider_session_id().as_deref() == Some(SESSION)
        });
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    fn write(&self, rel: &str, bytes: &[u8], mode: u32) {
        write_file(&self.file(rel), bytes, mode);
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.file(rel)).unwrap()
    }

    fn pump(&mut self) {
        let project = self.project.clone();
        self.tabs.pump(&project, true);
    }

    fn snaps(&self) -> Vec<eitri_core::turn_review::TurnRecord> {
        self.tabs.turn_review().unwrap().turns(SESSION)
    }

    /// Starts a turn and waits until its base is taken; returns its number.
    fn start_turn(&mut self) -> u32 {
        self.turns += 1;
        let id = format!("t{}", self.turns);
        self.provider
            .queue(AgentDomainEvent::TurnStarted { turn_id: id.clone() });
        until("the turn's base", || {
            self.pump();
            self.snaps()
                .iter()
                .any(|r| r.turn_id == id && matches!(r.base, Snap::Taken { .. }))
        });
        self.snaps().iter().find(|r| r.turn_id == id).unwrap().n
    }

    fn end_turn(&mut self) {
        let id = format!("t{}", self.turns);
        self.provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: id.clone(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        until("the turn's end", || {
            self.pump();
            self.tabs.running_count() == 0
                && self
                    .snaps()
                    .iter()
                    .any(|r| r.turn_id == id && matches!(r.end, Snap::Taken { .. }))
        });
    }

    /// One whole turn whose change on disk is `change`; returns its number.
    fn turn(&mut self, change: impl FnOnce(&Fixture)) -> u32 {
        let n = self.start_turn();
        change(self);
        self.end_turn();
        n
    }

    /// Hands `message` to the flow.
    fn handle(&mut self, rpc: Option<&dyn EditorRpc>, message: InboundMessage) -> Vec<Out> {
        self.flow.handle(message, &mut self.tabs, rpc, Instant::now())
    }

    /// One pump and one tick.
    fn tick(&mut self, rpc: Option<&dyn EditorRpc>) -> Vec<Out> {
        self.pump();
        self.flow.tick(&mut self.tabs, rpc, Instant::now())
    }

    /// Ticks until the request `id` is answered, and returns everything sent meanwhile.
    fn drive(&mut self, rpc: Option<&dyn EditorRpc>, id: &str) -> Vec<Out> {
        let mut all = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            all.extend(self.tick(rpc));
            if answered(&all, id) {
                return all;
            }
            assert!(Instant::now() < deadline, "{id} was never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `message`, then ticks until it is answered; its immediate answers come first.
    fn run(&mut self, rpc: Option<&dyn EditorRpc>, id: &str, message: InboundMessage) -> Vec<Out> {
        let mut all = self.handle(rpc, message);
        if !answered(&all, id) {
            all.extend(self.drive(rpc, id));
        }
        all
    }

    fn refs(&self, prefix: &str) -> Vec<String> {
        let out = tgit(
            &self.review_dir,
            &["--git-dir=git", "for-each-ref", "--format=%(refname)", prefix],
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn draft(&self) -> &eitri_core::turn_review::ReviewDraft {
        self.tabs.review_draft(self.tab).unwrap()
    }

    fn shut_down(mut self) {
        for tab in self.tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

const BASE: &[u8] = b"one\ntwo\nthree\n";
const CHANGED: &[u8] = b"one\nTWO\nthree\n";

/// A fixture whose `a.txt` changed in one turn; returns it with the turn's number.
fn changed_once(name: &str) -> (Fixture, u32) {
    let mut f = Fixture::new(name);
    f.write("a.txt", BASE, 0o644);
    let n = f.turn(|f| f.write("a.txt", CHANGED, 0o644));
    (f, n)
}

// ---- revert ----------------------------------------------------------------------------------------

#[test]
fn a_revert_waits_for_the_editor_then_lands_in_the_draft() {
    let (mut f, n) = changed_once("lands");
    let editor = FakeEditor::holding();
    let outs = f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt"));
    assert!(
        outs.is_empty(),
        "nothing is answered while the editor is asked: {:?}",
        shape(&outs)
    );
    assert!(!f.flow.is_idle());
    assert!(f.flow.owes_answer("r1"), "the request is still to be answered");
    assert!(!f.flow.owes_answer("r2"));
    assert_eq!(editor.asked(), 1);
    let path_asked = match &editor.asked.borrow()[0].1[0] {
        Value::String(s) => s.as_str().unwrap().to_string(),
        other => panic!("the path went as {other:?}"),
    };
    assert_eq!(path_asked, f.file("a.txt").to_string_lossy());
    for _ in 0..5 {
        assert!(f.tick(editor.rpc()).is_empty(), "still waiting");
    }
    assert_eq!(
        f.read("a.txt"),
        CHANGED,
        "nothing is written before the editor clears the file"
    );
    assert_eq!(f.flow.jobs_started(), 0);

    editor.release(clear_buffer());
    let outs = f.drive(editor.rpc(), "r1");
    assert_eq!(shape(&outs), ["review_draft:null", "command_result:r1:ok"]);
    assert_eq!(f.read("a.txt"), BASE);
    let reverts = f.draft().reverts();
    assert_eq!(reverts.len(), 1);
    assert_eq!(reverts[0].new.source, RevertSource::Panel);
    assert_eq!(reverts[0].new.shape, RevertShape::WholeFile);
    assert_eq!(reverts[0].new.path, "a.txt");
    assert_eq!(reverts[0].new.reverted_to, BASE);
    assert_eq!(reverts[0].new.replaced, CHANGED);
    assert!(reverts[0].new.undo.is_some());
    assert!(f.flow.is_idle(), "the slot is free again");
    assert!(!f.flow.owes_answer("r1"), "answered");
    let draft = &envelopes(&outs)[0]["draft"];
    assert_eq!(draft["canUndo"], true);
    assert_eq!(draft["reverts"][0]["what"], "file");
    f.shut_down();
}

#[test]
fn a_hunk_revert_records_the_lines_it_covers() {
    let (mut f, n) = changed_once("hunk");
    let diff = f
        .tabs
        .review_diff_job(f.tab, n, Scope::Turn, "a.txt")
        .unwrap()
        .run()
        .unwrap();
    let hunk = diff.hunks.unwrap().remove(0);
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.run(
        editor.rpc(),
        "r1",
        revert_hunk("r1", f.tab, n, "a.txt", hunk.id, &hunk.header),
    );
    assert_eq!(result_of(&outs, "r1"), Ok(None));
    assert_eq!(f.read("a.txt"), BASE);
    let record = &f.draft().reverts()[0].new;
    assert_eq!(record.hunk, Some((hunk.id, hunk.header.clone())));
    let lines = record.reverted_to.split_inclusive(|b| *b == b'\n').count() as u32;
    assert_eq!(
        record.shape,
        RevertShape::Lines {
            from: record.at_line,
            to: record.at_line + lines - 1
        }
    );
    f.shut_down();
}

#[test]
fn no_editor_refuses_and_nothing_is_written() {
    let (mut f, n) = changed_once("no-editor");
    let before = fingerprint(&f.file("a.txt"));
    let outs = f.run(None, "r1", revert_file("r1", f.tab, n, "a.txt"));
    let why = result_of(&outs, "r1").unwrap_err();
    assert!(
        why.starts_with("no editor is connected, so Eitri cannot tell whether"),
        "{why}"
    );
    assert_eq!(fingerprint(&f.file("a.txt")), before);

    // An editor that is not connected (no target) is the same refusal.
    let gone = FakeEditor::answering(clear_buffer());
    gone.target.set(None);
    let outs = f.run(gone.rpc(), "r2", revert_file("r2", f.tab, n, "a.txt"));
    assert!(result_of(&outs, "r2")
        .unwrap_err()
        .starts_with("no editor is connected"));
    assert_eq!(gone.asked(), 0, "a missing editor is not asked");
    assert_eq!(fingerprint(&f.file("a.txt")), before);
    assert_eq!(f.flow.jobs_started(), 0);
    assert!(f.flow.is_idle());
    f.shut_down();
}

#[test]
fn a_running_turn_in_this_window_refuses_before_asking_the_editor() {
    let (mut f, n) = changed_once("running");
    f.start_turn();
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt"));
    assert_eq!(result_of(&outs, "r1"), Err("an agent turn is running".to_string()));
    assert_eq!(editor.asked(), 0, "the editor is not asked while a turn runs");
    assert_eq!(f.read("a.txt"), CHANGED);
    f.end_turn();
    f.shut_down();
}

#[test]
fn a_revert_for_a_tab_closed_meanwhile_is_refused() {
    let (mut f, n) = changed_once("tab-closed");
    let spare = f.tabs.open(eitri_core::tab_set::AgentId::Claude);
    let editor = FakeEditor::holding();
    let outs = f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt"));
    assert!(outs.is_empty());
    let before = fingerprint(&f.file("a.txt"));

    let mut closed = f.tabs.remove(f.tab).expect("the tab was open");
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    assert_eq!(f.tabs.active(), spare);
    editor.release(clear_buffer());
    let outs = f.drive(editor.rpc(), "r1");
    assert_eq!(
        result_of(&outs, "r1"),
        Err("the tab closed; nothing was written".to_string())
    );
    assert_eq!(f.flow.jobs_started(), 0, "no job was ever built");
    assert_eq!(
        fingerprint(&f.file("a.txt")),
        before,
        "bytes, inode and mtime are as they were"
    );
    assert!(f.refs("refs/eitri-revert/").is_empty(), "nothing was anchored");
    assert!(f.flow.is_idle(), "the slot is free");
    f.shut_down();
}

#[test]
fn a_session_change_meanwhile_is_refused() {
    let (mut f, n) = changed_once("session-change");
    let editor = FakeEditor::holding();
    assert!(f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt")).is_empty());
    let before = fingerprint(&f.file("a.txt"));

    // The tab now holds another session: its backend is swapped for one that opened a different id.
    let (other, backend) = live_backend(&f.project);
    let old = f.tabs.get_mut(f.tab).unwrap().set_backend(TabBackend::Live(backend));
    if let TabBackend::Live(mut old) = old {
        old.shutdown();
    }
    other.open_session("another-session", &f.project);
    let tab = f.tab;
    until("the other session", || {
        f.tabs.get(tab).unwrap().provider_session_id().as_deref() == Some("another-session")
    });

    editor.release(clear_buffer());
    let outs = f.drive(editor.rpc(), "r1");
    assert_eq!(
        result_of(&outs, "r1"),
        Err("the tab's session changed; nothing was written".to_string())
    );
    assert_eq!(f.flow.jobs_started(), 0);
    assert_eq!(fingerprint(&f.file("a.txt")), before);
    assert!(f.flow.is_idle());
    f.shut_down();
}

#[test]
fn a_changed_editor_refuses_an_answer_meant_for_the_one_asked() {
    let (mut f, n) = changed_once("editor-changed");
    let editor = FakeEditor::holding();
    assert!(f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt")).is_empty());
    let before = fingerprint(&f.file("a.txt"));
    // nvim restarted, or the panel attached to another one, while the question was out.
    editor.target.set(Some(8));
    editor.release(clear_buffer());
    let outs = f.drive(editor.rpc(), "r1");
    assert_eq!(
        result_of(&outs, "r1"),
        Err("the editor changed while it was asked; nothing was written".to_string())
    );
    assert_eq!(f.flow.jobs_started(), 0);
    assert_eq!(fingerprint(&f.file("a.txt")), before);
    f.shut_down();
}

#[test]
fn a_second_write_is_refused_while_one_is_out() {
    let (mut f, n) = changed_once("one-write");
    let editor = FakeEditor::holding();
    assert!(f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt")).is_empty());
    let outs = f.handle(editor.rpc(), revert_file("r2", f.tab, n, "a.txt"));
    assert_eq!(
        result_of(&outs, "r2"),
        Err("a revert is still being written".to_string())
    );
    let outs = f.handle(editor.rpc(), undo("u1", f.tab));
    assert_eq!(
        result_of(&outs, "u1"),
        Err("a revert is still being written".to_string())
    );
    assert_eq!(editor.asked(), 1, "the second request never asked the editor");
    // The first is unharmed.
    editor.release(clear_buffer());
    let outs = f.drive(editor.rpc(), "r1");
    assert_eq!(result_of(&outs, "r1"), Ok(None));
    assert_eq!(f.read("a.txt"), BASE);
    f.shut_down();
}

#[test]
fn a_path_outside_the_project_is_not_even_asked_about() {
    let (mut f, n) = changed_once("outside");
    let editor = FakeEditor::answering(clear_buffer());
    for (i, path) in ["../escape.txt", "/etc/passwd", "a/../../x", ""]
        .into_iter()
        .enumerate()
    {
        let id = format!("r{i}");
        let outs = f.handle(editor.rpc(), revert_file(&id, f.tab, n, path));
        let why = result_of(&outs, &id).unwrap_err();
        assert!(why.contains("is not a path inside the project"), "{path:?}: {why}");
    }
    assert_eq!(editor.asked(), 0);
    f.shut_down();
}

#[test]
fn a_command_that_names_no_tab_is_refused_not_sent_to_the_active_one() {
    let (mut f, _n) = changed_once("no-tab");
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.handle(editor.rpc(), msg(json!({"type":"review_undo","request_id":"u1"})));
    assert_eq!(
        result_of(&outs, "u1"),
        Err("protocol: this command names no tab".to_string())
    );
    let outs = f.handle(editor.rpc(), undo("u2", TabId(999)));
    assert_eq!(result_of(&outs, "u2"), Err("protocol: no tab 999".to_string()));
    f.shut_down();
}

#[test]
fn a_window_that_holds_no_presence_refuses_every_write() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let scratch = Scratch::new("no-presence");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let review_dir = review_dir_for(Some(scratch.0.join("state").as_os_str()), None, &project).unwrap();
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let options = ReviewOptions {
        excludes: Some(None),
        trim_every: Duration::MAX,
        ..ReviewOptions::default()
    };
    tabs.install_turn_review(TurnReview::with_options(Some(review_dir), &project, options))
        .unwrap();
    let tab = tabs.active();
    let (provider, backend) = live_backend(&project);
    tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.open_session(SESSION, &project);
    until("the session", || {
        tabs.pump(&project, true);
        tabs.get(tab).unwrap().provider_session_id().as_deref() == Some(SESSION)
    });
    let mut flow = ReviewFlow::new();
    let editor = FakeEditor::answering(clear_buffer());
    let outs = flow.handle(
        revert_file("r1", tab, 1, "a.txt"),
        &mut tabs,
        editor.rpc(),
        Instant::now(),
    );
    let why = result_of(&outs, "r1").unwrap_err();
    assert!(why.starts_with("this window's presence lock is not held"), "{why}");
    assert_eq!(editor.asked(), 0);
    for tab in tabs.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

// ---- undo ------------------------------------------------------------------------------------------

#[test]
fn undo_marks_the_record_undone() {
    let (mut f, n) = changed_once("undo");
    let editor = FakeEditor::answering(clear_buffer());
    f.run(editor.rpc(), "r1", revert_file("r1", f.tab, n, "a.txt"));
    assert_eq!(f.read("a.txt"), BASE);
    assert!(f.draft().last_undoable().is_some());

    let outs = f.run(editor.rpc(), "u1", undo("u1", f.tab));
    assert_eq!(shape(&outs), ["review_draft:null", "command_result:u1:ok"]);
    assert_eq!(f.read("a.txt"), CHANGED, "the agent's change is back");
    assert!(f.draft().reverts()[0].undone);
    assert!(f.draft().last_undoable().is_none());
    assert_eq!(envelopes(&outs)[0]["draft"]["canUndo"], false);

    let outs = f.handle(editor.rpc(), undo("u2", f.tab));
    assert_eq!(result_of(&outs, "u2"), Err("nothing to undo".to_string()));
    f.shut_down();
}

#[test]
fn undo_restores_kind_and_mode_through_the_draft() {
    let mut f = Fixture::new("undo-mode");
    let n = f.turn(|f| f.write("run.sh", b"#!/bin/sh\necho hi\n", 0o755));
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.run(editor.rpc(), "r1", revert_file("r1", f.tab, n, "run.sh"));
    assert_eq!(result_of(&outs, "r1"), Ok(None));
    assert!(!f.file("run.sh").exists(), "the file the turn created is gone");
    assert_eq!(f.draft().reverts()[0].new.shape, RevertShape::Deleted);
    // What the draft kept to undo it with is the file as it was: executable, with its bytes.
    let undo_data = f.draft().reverts()[0].new.undo.clone().unwrap();
    assert!(matches!(undo_data.pre, UndoState::Regular { mode: 0o755, .. }));
    assert_eq!(undo_data.post, UndoState::Absent);

    let outs = f.run(editor.rpc(), "u1", undo("u1", f.tab));
    assert_eq!(result_of(&outs, "u1"), Ok(None));
    assert_eq!(f.read("run.sh"), b"#!/bin/sh\necho hi\n");
    assert_eq!(mode_of(&f.file("run.sh")), 0o755);
    assert!(f.draft().last_undoable().is_none());
    f.shut_down();
}

#[test]
fn the_draft_and_the_engine_convert_a_path_state_without_losing_kind_or_mode() {
    let cases = [
        Saved::Regular {
            blob: "abc".into(),
            mode: 0o755,
        },
        Saved::Regular {
            blob: "def".into(),
            mode: 0o4640,
        },
        Saved::Symlink { blob: "ghi".into() },
        Saved::Absent,
    ];
    for saved in cases {
        assert_eq!(undo_to_saved(saved_to_undo(saved.clone())), saved);
    }
}

// ---- comments --------------------------------------------------------------------------------------

#[test]
fn a_comment_is_quoted_from_the_end_snapshot_and_can_be_removed() {
    let (mut f, n) = changed_once("comment");
    let outs = f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "why this?"));
    assert_eq!(shape(&outs), ["review_draft:c1"]);
    let draft = &envelopes(&outs)[0]["draft"];
    assert_eq!(draft["comments"][0]["text"], "why this?");
    assert_eq!(draft["comments"][0]["anchor"], json!(["one"]));
    let id = draft["comments"][0]["id"].as_u64().unwrap();

    let outs = f.handle(
        None,
        msg(json!({"type":"review_comment_remove","request_id":"c2","tab":f.tab.0,"id":id})),
    );
    assert_eq!(shape(&outs), ["review_draft:c2"]);
    assert!(f.draft().is_empty());
    let outs = f.handle(
        None,
        msg(json!({"type":"review_comment_remove","request_id":"c3","tab":f.tab.0,"id":id})),
    );
    assert!(result_of(&outs, "c3").is_err(), "no such comment now");

    let outs = f.run(None, "c4", comment("c4", f.tab, n, "a.txt", "  "));
    assert!(result_of(&outs, "c4").unwrap_err().contains("empty"));
    f.shut_down();
}

#[test]
fn a_comment_whose_tab_closed_while_it_was_quoted_is_refused() {
    let (mut f, n) = changed_once("comment-closed");
    let _spare = f.tabs.open(eitri_core::tab_set::AgentId::Claude);
    let mut outs = f.handle(None, comment("c1", f.tab, n, "a.txt", "hello"));
    let mut closed = f.tabs.remove(f.tab).unwrap();
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    if !answered(&outs, "c1") {
        outs.extend(f.drive(None, "c1"));
    }
    assert!(result_of(&outs, "c1").unwrap_err().starts_with("the tab closed"));
    f.shut_down();
}

// ---- send ------------------------------------------------------------------------------------------

/// The preview envelope among `outs`.
fn preview_of(outs: &[Out]) -> Json {
    envelopes(outs)
        .into_iter()
        .find(|v| v["kind"] == "review_send_preview")
        .unwrap_or_else(|| panic!("no preview in {:?}", shape(outs)))
}

#[test]
fn send_preview_then_confirm_sends_once_and_clears_the_draft() {
    let (mut f, n) = changed_once("send");
    f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "why this?"));

    let outs = f.run(None, "s1", send("s1", f.tab, None));
    assert_eq!(shape(&outs), ["review_send_preview:s1"]);
    let preview = preview_of(&outs);
    let digest = preview["digest"].as_str().unwrap().to_string();
    let text = preview["text"].as_str().unwrap().to_string();
    assert!(text.contains("why this?"));
    assert_eq!(preview["queued"], false);
    assert!(f.provider.turns().is_empty(), "a preview sends nothing");
    assert!(!f.draft().is_empty());

    let outs = f.run(None, "s2", send("s2", f.tab, Some(&digest)));
    assert_eq!(shape(&outs), ["flushed", "review_draft:null", "command_result:s2:ok"]);
    assert_eq!(result_of(&outs, "s2"), Ok(Some("sent".to_string())));
    assert_eq!(
        f.provider.turns(),
        vec![text],
        "exactly the message that was previewed, once"
    );
    assert!(f.draft().is_empty());
    assert_eq!(envelopes(&outs)[0]["draft"]["comments"], json!([]));
    f.shut_down();
}

#[test]
fn a_stale_digest_is_refused() {
    let (mut f, n) = changed_once("stale");
    let editor = FakeEditor::answering(clear_buffer());
    f.run(editor.rpc(), "r1", revert_file("r1", f.tab, n, "a.txt"));
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    let digest = preview_of(&outs)["digest"].as_str().unwrap().to_string();

    // The disk moved on after the preview: the reverted file is not what the preview described.
    f.write("a.txt", b"something else entirely\n", 0o644);
    let outs = f.run(None, "s2", send("s2", f.tab, Some(&digest)));
    assert_eq!(
        result_of(&outs, "s2"),
        Err("the draft or the disk changed since the preview; press s again".to_string())
    );
    assert!(f.provider.turns().is_empty());
    assert!(!f.draft().is_empty(), "the draft is kept");
    f.shut_down();
}

#[test]
fn a_digest_that_was_never_shown_is_refused() {
    let (mut f, n) = changed_once("wrong-digest");
    f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "x"));
    let outs = f.run(None, "s1", send("s1", f.tab, Some("0000000000000000")));
    assert!(result_of(&outs, "s1").unwrap_err().contains("press s again"));
    assert!(f.provider.turns().is_empty());
    f.shut_down();
}

#[test]
fn send_while_a_turn_runs_is_queued() {
    let (mut f, n) = changed_once("send-queued");
    f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "later"));
    f.start_turn();
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    let preview = preview_of(&outs);
    assert_eq!(preview["queued"], true, "the tab is busy, so a confirmed send waits");
    let digest = preview["digest"].as_str().unwrap().to_string();

    let outs = f.run(None, "s2", send("s2", f.tab, Some(&digest)));
    assert_eq!(
        shape(&outs),
        ["queue_changed", "review_draft:null", "command_result:s2:ok"],
        "the queued review is announced so the page's queue strip shows it"
    );
    assert!(outs
        .iter()
        .any(|out| matches!(out, Out::QueueChanged(tab) if *tab == f.tab)));
    assert_eq!(
        result_of(&outs, "s2"),
        Ok(Some("queued: it is sent when the running turn ends".to_string()))
    );
    assert!(f.provider.turns().is_empty(), "nothing goes out while the turn runs");
    assert_eq!(f.tabs.queued_count(), 1);
    assert!(f.draft().is_empty());

    // The turn ends: the tab's own queue flush sends it, as it would any typed message.
    f.end_turn();
    let flush = f
        .tabs
        .flush_queue(f.tab)
        .expect("the queue goes out once the turn is over");
    assert!(flush.outcome.is_ok());
    assert_eq!(f.provider.turns().len(), 1);
    f.shut_down();
}

#[test]
fn an_empty_draft_is_not_sent() {
    let (mut f, _n) = changed_once("send-empty");
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    assert!(result_of(&outs, "s1").unwrap_err().contains("nothing to send"));
    f.shut_down();
}

#[test]
fn a_draft_with_nothing_on_disk_is_previewed_but_not_sent() {
    let (mut f, n) = changed_once("send-undone-only");
    let editor = FakeEditor::answering(clear_buffer());
    f.run(editor.rpc(), "r1", revert_file("r1", f.tab, n, "a.txt"));
    f.run(editor.rpc(), "u1", undo("u1", f.tab));
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    let preview = preview_of(&outs);
    assert_eq!(preview["notOnDisk"][0]["why"], "undone");
    let digest = preview["digest"].as_str().unwrap().to_string();
    let outs = f.run(None, "s2", send("s2", f.tab, Some(&digest)));
    assert!(result_of(&outs, "s2")
        .unwrap_err()
        .contains("nothing in the draft can be sent"));
    assert!(f.provider.turns().is_empty());
    f.shut_down();
}

#[test]
fn a_send_waits_out_a_write_and_a_write_waits_out_a_send() {
    let (mut f, n) = changed_once("send-vs-write");
    f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "x"));
    let editor = FakeEditor::holding();
    assert!(f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt")).is_empty());
    let outs = f.handle(None, send("s1", f.tab, None));
    assert!(result_of(&outs, "s1")
        .unwrap_err()
        .contains("a revert is still being written"));
    editor.release(clear_buffer());
    f.drive(editor.rpc(), "r1");

    // And the other way round: a message being prepared from the disk holds writes back. Its own
    // answer may arrive during any of the ticks the calls below run, so all of them are kept.
    let editor = FakeEditor::answering(clear_buffer());
    let mut all = f.handle(None, send("s2", f.tab, None));
    let refused = f.handle(editor.rpc(), undo("u1", f.tab));
    assert!(result_of(&refused, "u1").unwrap_err().contains("still being prepared"));
    all.extend(refused);
    if !answered(&all, "s2") {
        all.extend(f.drive(None, "s2"));
    }
    assert_eq!(preview_of(&all)["requestId"], "s2");
    f.shut_down();
}

/// A revert made in the editor that is not on disk, and what the buffer holds of it: the message
/// names it for what it is, whatever the buffer's line endings are.
fn only_in_the_editor(name: &str, on_disk: &[u8], reverted_to: &[u8], buffer: Value) {
    let (mut f, n) = changed_once(name);
    f.write("e.txt", on_disk, 0o644);
    f.tabs.review_draft_mut(f.tab).unwrap().record_revert(NewRevert {
        turn: n,
        path: "e.txt".into(),
        hunk: None,
        shape: RevertShape::Lines { from: 1, to: 2 },
        source: RevertSource::Editor,
        at_line: 1,
        reverted_to: reverted_to.to_vec(),
        replaced: on_disk.to_vec(),
        undo: None,
    });
    let editor = FakeEditor::answering(buffer);
    let outs = f.run(editor.rpc(), "s1", send("s1", f.tab, None));
    let preview = preview_of(&outs);
    assert_eq!(
        preview["notOnDisk"][0]["why"], "only in the editor, not saved",
        "{preview}"
    );
    assert_eq!(preview["notOnDisk"][0]["path"], "e.txt");
    assert_eq!(editor.asked(), 1);
    // The range asked about is the revert's own: where it starts and how many lines it has.
    let asked = editor.asked.borrow();
    assert_eq!(asked[0].1[1], Value::Array(vec![Value::from(1u32), Value::from(2u32)]));
    assert_eq!(f.read("e.txt"), on_disk, "asking changes no file");
    drop(asked);
    f.shut_down();
}

#[test]
fn an_editor_revert_not_written_is_named_in_the_preview_for_a_dos_buffer() {
    only_in_the_editor(
        "editor-dos",
        b"old1\r\nold2\r\n",
        b"alpha\r\nbeta\r\n",
        buffer_with(&["alpha", "beta"], "dos", true, true),
    );
}

#[test]
fn an_editor_revert_not_written_is_named_in_the_preview_for_a_noeol_buffer() {
    only_in_the_editor(
        "editor-noeol",
        b"old1\nold2",
        b"alpha\nbeta",
        buffer_with(&["alpha", "beta"], "unix", false, true),
    );
}

#[test]
fn an_editor_revert_the_editor_cannot_vouch_for_is_never_called_only_in_the_editor() {
    // No editor answer: the status falls back to what the disk says, not to a guess about a buffer.
    let (mut f, n) = changed_once("editor-silent");
    f.write("e.txt", b"old1\nold2\n", 0o644);
    f.tabs.review_draft_mut(f.tab).unwrap().record_revert(NewRevert {
        turn: n,
        path: "e.txt".into(),
        hunk: None,
        shape: RevertShape::Lines { from: 1, to: 2 },
        source: RevertSource::Editor,
        at_line: 1,
        reverted_to: b"alpha\nbeta\n".to_vec(),
        replaced: b"old1\nold2\n".to_vec(),
        undo: None,
    });
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    assert_eq!(preview_of(&outs)["notOnDisk"][0]["why"], "undone");
    f.shut_down();
}

// ---- recovery --------------------------------------------------------------------------------------

/// The size of the crash child's new content: over one write chunk, so the abort leaves a file
/// that is genuinely half written.
const CRASH_NEW_LEN: usize = 200 * 1024;

/// The child of the recovery tests: an in-place rewrite of a file with an extended attribute that dies after its
/// first chunk, as a crash of a window in the middle of a revert would.
#[test]
#[ignore = "run by the recovery tests"]
fn flow_crash_child() {
    let (Some(dir), Some(rel)) = (std::env::var_os(CHILD_DIR), std::env::var_os(CHILD_REL)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let rel = PathBuf::from(rel);
    let project = dir.join("project").canonicalize().unwrap();
    let review_dir = review_dir_for(Some(dir.join("state").as_os_str()), None, &project).unwrap();
    let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();
    let journal = Journal::open(&review_dir).unwrap();
    let pre = std::fs::read(project.join(&rel)).unwrap();
    let new: Vec<u8> = (0..CRASH_NEW_LEN).map(|i| (i % 251) as u8).collect();
    let note = JournalNote {
        session: SESSION.into(),
        path: rel.clone(),
        pre: Some(shadow.store_blob(&pre).unwrap()),
        pre_mode: mode_of(&project.join(&rel)),
        intended: Some(shadow.store_blob(&new).unwrap()),
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `no_core` is a valid `rlimit`; no dump is wanted from the abort below.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) };
    let hooks = FsHooks {
        fault: Some(Arc::new(|stage| {
            if matches!(stage, Stage::Wrote(n) if n >= 64 * 1024) {
                std::process::abort();
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
    let target = ProjectDir::open(&project).unwrap().target(&rel, false).unwrap();
    let content = Content::Bytes {
        bytes: new,
        mode: 0o644,
    };
    let _ = replace(&target, &content, &note, &journal, &shadow, &hooks, None);
    unreachable!("the write was to abort");
}

/// One crash test at a time. A journal entry is read under its own `flock`, which a process forked
/// at that instant (another test's child, or a `git` of another test's worker) holds on to until
/// it execs; the journal is read once per window, so a read that lands there misses the entry.
/// Three tests forking and reading at once make that likely; one at a time make it rare.
fn one_crash_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static CRASH: std::sync::Mutex<()> = std::sync::Mutex::new(());
    CRASH.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Kills an in-place write of `rel` halfway (the file gets an extended attribute, so it is
/// rewritten in place) and returns the id of the journal entry it left.
fn crash_mid_write(f: &Fixture, rel: &str) -> String {
    let c = std::ffi::CString::new(f.file(rel).as_os_str().as_encoded_bytes()).unwrap();
    let name = std::ffi::CString::new("user.eitri_test").unwrap();
    #[cfg(target_vendor = "apple")]
    // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
    let set = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0, 0) };
    #[cfg(not(target_vendor = "apple"))]
    // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
    let set = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0) };
    assert_eq!(set, 0, "these tests need user extended attributes");
    let original = f.read(rel);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "flow_crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, &f.scratch.0)
        .env(CHILD_REL, rel)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the crash child did not finish in 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "the child was to die");
    assert_ne!(f.read(rel), original, "the file is half written");
    // The kernel lets go of the dead writer's lock a moment after it exits.
    let journal = Journal::open(&f.review_dir).unwrap();
    let mut found = None;
    until("the entry the crash left", || {
        found = journal
            .pending()
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.path == Path::new(rel));
        found.is_some()
    });
    found.unwrap().id
}

fn pending_ids(f: &Fixture) -> Vec<String> {
    Journal::open(&f.review_dir)
        .unwrap()
        .pending()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// The entries of the newest `review_recovery` envelope among `outs`.
fn offered(outs: &[Out]) -> Option<Vec<String>> {
    envelopes(outs)
        .into_iter()
        .rev()
        .find(|v| v["kind"] == "review_recovery")
        .map(|v| {
            v["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect()
        })
}

/// Ticks until the recoveries the journal holds have been offered. The journal is read once per
/// window, under each entry's own lock, and an entry another process holds the lock of at that
/// instant (a `git` forked by another test's worker has a copy of it until it execs) is left out of
/// that read. A window that missed an entry finds it at its next start; here a fresh flow stands
/// for that start, a few times, so that the flow's behaviour with an offer is what is tested.
fn first_offer(f: &mut Fixture) -> Vec<String> {
    for _attempt in 0..5 {
        let mut outs = f.flow.document_ready(&f.tabs);
        let deadline = Instant::now() + Duration::from_secs(2);
        while offered(&outs).is_none() && Instant::now() < deadline {
            outs.extend(f.tick(None));
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(entries) = offered(&outs) {
            return entries;
        }
        f.flow = ReviewFlow::new();
    }
    panic!("the interrupted revert was never offered");
}

#[test]
fn recoveries_are_offered_once_at_ready_and_again_from_memory_after_a_reload() {
    let _one = one_crash_at_a_time();
    let mut f = Fixture::new("recover-offer");
    f.write("a.txt", b"the original\n", 0o644);
    let id = crash_mid_write(&f, "a.txt");

    assert_eq!(first_offer(&mut f), vec![id.clone()]);
    assert_eq!(f.flow.jobs_started(), 1, "the journal was read once");
    // A reload of the page: told again, and the journal is not read again.
    let outs = f.flow.document_ready(&f.tabs);
    assert_eq!(offered(&outs), Some(vec![id.clone()]));
    assert_eq!(f.flow.jobs_started(), 1);
    f.shut_down();
}

#[test]
fn nothing_is_offered_when_nothing_was_interrupted() {
    let mut f = Fixture::new("recover-none");
    let outs = f.flow.document_ready(&f.tabs);
    assert!(outs.is_empty());
    for _ in 0..20 {
        let outs = f.tick(None);
        assert!(offered(&outs).is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    f.settle_file_jobs(None);
    assert!(f.flow.document_ready(&f.tabs).is_empty());
    assert_eq!(f.flow.jobs_started(), 1);
    f.shut_down();
}

#[test]
fn recover_is_refused_while_a_turn_runs_or_the_buffer_is_modified() {
    let _one = one_crash_at_a_time();
    let mut f = Fixture::new("recover-refused");
    f.write("a.txt", b"the original\n", 0o644);
    let id = crash_mid_write(&f, "a.txt");
    first_offer(&mut f);
    let half = f.read("a.txt");

    // (c) no editor.
    let outs = f.run(None, "v1", recover("v1", &id, "restore"));
    assert!(result_of(&outs, "v1")
        .unwrap_err()
        .starts_with("no editor is connected"));

    // (b) an editor that holds unsaved changes.
    let editor = FakeEditor::answering(modified_buffer());
    let outs = f.run(editor.rpc(), "v2", recover("v2", &id, "restore"));
    assert!(result_of(&outs, "v2")
        .unwrap_err()
        .contains("has unsaved changes in the editor"));
    assert_eq!(editor.asked(), 1);

    // (a) a turn running in this window: refused before the editor is asked.
    f.start_turn();
    let fresh = FakeEditor::answering(clear_buffer());
    let outs = f.handle(fresh.rpc(), recover("v3", &id, "restore"));
    assert_eq!(result_of(&outs, "v3"), Err("an agent turn is running".to_string()));
    assert_eq!(fresh.asked(), 0);
    f.end_turn();

    assert_eq!(f.read("a.txt"), half, "no refusal wrote anything");
    assert_eq!(pending_ids(&f), vec![id.clone()], "the entry is still pending");

    // An id that was never offered is refused too.
    let outs = f.handle(fresh.rpc(), recover("v4", "not-an-entry", "restore"));
    assert!(result_of(&outs, "v4").unwrap_err().contains("not on offer"));

    // Letting it go still works, and the notice is taken away.
    let outs = f.run(None, "v5", recover("v5", &id, "dismiss"));
    assert_eq!(shape(&outs), ["review_recovery:null", "command_result:v5:ok"]);
    assert_eq!(offered(&outs), Some(Vec::new()), "an empty list clears the notice");
    assert!(pending_ids(&f).is_empty());
    assert_eq!(f.read("a.txt"), half, "forgetting writes nothing");
    f.shut_down();
}

#[test]
fn a_restore_puts_the_file_back_and_clears_the_offer() {
    let _one = one_crash_at_a_time();
    let mut f = Fixture::new("recover-restore");
    f.write("a.txt", b"the original\n", 0o644);
    let id = crash_mid_write(&f, "a.txt");
    first_offer(&mut f);
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.run(editor.rpc(), "v1", recover("v1", &id, "restore"));
    assert_eq!(result_of(&outs, "v1"), Ok(None));
    assert_eq!(f.read("a.txt"), b"the original\n");
    assert_eq!(offered(&outs), Some(Vec::new()));
    assert!(pending_ids(&f).is_empty());
    f.shut_down();
}

#[test]
fn a_restore_of_a_finished_write_says_nothing_was_changed() {
    let _one = one_crash_at_a_time();
    let mut f = Fixture::new("recover-finished");
    f.write("a.txt", b"the original\n", 0o644);
    let id = crash_mid_write(&f, "a.txt");
    first_offer(&mut f);
    // The write had in fact finished; only its journal entry was left.
    let intended: Vec<u8> = (0..CRASH_NEW_LEN).map(|i| (i % 251) as u8).collect();
    f.write("a.txt", &intended, 0o644);
    let editor = FakeEditor::answering(clear_buffer());
    let outs = f.run(editor.rpc(), "v1", recover("v1", &id, "restore"));
    assert_eq!(
        result_of(&outs, "v1"),
        Ok(Some(
            "a.txt already held what the interrupted revert was writing; nothing was changed, the entry is forgotten"
                .to_string()
        ))
    );
    assert_eq!(f.read("a.txt"), intended);
    assert_eq!(offered(&outs), Some(Vec::new()));
    assert!(pending_ids(&f).is_empty());
    f.shut_down();
}

#[test]
fn the_journal_is_read_at_the_first_ready_after_a_review_exists() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let scratch = Scratch::new("recover-late");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let review_dir = review_dir_for(Some(scratch.0.join("state").as_os_str()), None, &project).unwrap();
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let mut flow = ReviewFlow::new();
    // The page can say ready before `init.lua` has decided whether there is a review.
    assert!(flow.document_ready(&tabs).is_empty());
    assert_eq!(flow.jobs_started(), 0);
    let options = ReviewOptions {
        excludes: Some(None),
        trim_every: Duration::MAX,
        ..ReviewOptions::default()
    };
    tabs.install_turn_review(TurnReview::with_options(Some(review_dir), &project, options))
        .unwrap();
    assert!(flow.document_ready(&tabs).is_empty());
    assert_eq!(flow.jobs_started(), 1, "asked once the review exists");
    assert!(flow.document_ready(&tabs).is_empty());
    assert_eq!(flow.jobs_started(), 1, "and only once");
    // The journal is read on a worker; one still running after the test would create its
    // directory again inside the scratch that is being removed.
    until("the journal read to finish", || {
        flow.tick(&mut tabs, None, Instant::now());
        flow.is_idle()
    });
}

// ---- review off ------------------------------------------------------------------------------------

#[test]
fn review_off_still_holds_presence_and_says_off() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let scratch = Scratch::new("review-off");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let review_dir = review_dir_for(Some(scratch.0.join("state").as_os_str()), None, &project).unwrap();
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    tabs.turn_review_off();
    tabs.hold_presence(PresenceHolder::start(review_dir.clone(), std::process::id()));
    let tab = tabs.active();

    // Another window's revert sees this one.
    let theirs = PresenceGuard::for_tests(&review_dir, OTHER_PID, 0);
    until("the other window sees this one", || {
        theirs.check()
            == Err(Blocked::Busy(Busy::OtherWindow {
                pid: std::process::id(),
            }))
    });

    // And a request here answers that the review is off, for every review request.
    let off = eitri_core::tab_set::TURN_REVIEW_OFF;
    let mut flow = ReviewFlow::new();
    let editor = FakeEditor::answering(clear_buffer());
    for (id, message) in [
        ("r1", revert_file("r1", tab, 1, "a.txt")),
        ("u1", undo("u1", tab)),
        ("c1", comment("c1", tab, 1, "a.txt", "x")),
        ("s1", send("s1", tab, None)),
        ("v1", recover("v1", "whatever", "restore")),
    ] {
        let outs = flow.handle(message, &mut tabs, editor.rpc(), Instant::now());
        assert_eq!(result_of(&outs, id), Err(off.to_string()), "{id}");
    }
    assert_eq!(
        tabs.review_overview_job(tab, eitri_core::turn_review::TurnRef::Latest, Scope::Turn)
            .err()
            .as_deref(),
        Some(off)
    );
    assert_eq!(editor.asked(), 0);
    assert!(flow.document_ready(&tabs).is_empty(), "no review, no journal to read");
    assert_eq!(flow.jobs_started(), 0);
}

// ---- the editor overlay ----------------------------------------------------------------------------

/// An editor that runs the review module's calls: it installs, opens, draws and clears, and hands
/// out the events a test queued. Every question is recorded.
struct ScriptedEditor {
    asked: RefCell<Vec<(&'static str, Vec<Value>)>>,
    events: RefCell<Vec<Value>>,
    target: Cell<Option<u64>>,
    /// What the editor says when it opens a file.
    opened: RefCell<Value>,
    /// Files no window shows, as the shown question is answered.
    hidden: RefCell<Vec<PathBuf>>,
    /// A live drawing finds an overlay it may not replace, and leaves it.
    left: Cell<bool>,
    /// Calls whose answers wait in `held` until [`ScriptedEditor::release`].
    hold: RefCell<Vec<&'static str>>,
    held: RefCell<Vec<(Answer, Value)>>,
}

fn vmap(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

impl ScriptedEditor {
    fn new() -> ScriptedEditor {
        ScriptedEditor {
            asked: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
            target: Cell::new(Some(7)),
            opened: RefCell::new(vmap(vec![
                ("opened", Value::from(true)),
                ("drawn", Value::from(1)),
                ("skipped", Value::from(0)),
                ("active", Value::from(1)),
            ])),
            hidden: RefCell::new(Vec::new()),
            left: Cell::new(false),
            hold: RefCell::new(Vec::new()),
            held: RefCell::new(Vec::new()),
        }
    }

    /// Gives every held answer, in the order the calls came.
    fn release(&self) {
        for (reply, answer) in self.held.borrow_mut().drain(..) {
            reply.send(Ok(answer));
        }
    }

    fn rpc(&self) -> Option<&dyn EditorRpc> {
        Some(self)
    }

    fn count(&self, code: &str) -> usize {
        self.asked.borrow().iter().filter(|(c, _)| *c == code).count()
    }

    /// The arguments of the `n`th call of `code`.
    fn args(&self, code: &str, n: usize) -> Vec<Value> {
        self.asked
            .borrow()
            .iter()
            .filter(|(c, _)| *c == code)
            .nth(n)
            .unwrap_or_else(|| panic!("no call {n} of that code"))
            .1
            .clone()
    }

    fn queue_event(&self, event: Value) {
        self.events.borrow_mut().push(event);
    }
}

impl EditorRpc for ScriptedEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        self.asked.borrow_mut().push((code, args.clone()));
        let answer = if code == REVIEW_LUA {
            vmap(vec![("installed", Value::from(true)), ("replaced", Value::from(false))])
        } else if code == OPEN_AND_SHOW_LUA {
            self.opened.borrow().clone()
        } else if code == SHOW_LUA {
            vmap(vec![("drawn", Value::from(1)), ("active", Value::from(1))])
        } else if code == CLEAR_LUA {
            vmap(vec![("active", Value::from(0))])
        } else if code == TAKE_EVENTS_LUA {
            let events: Vec<Value> = self.events.borrow_mut().drain(..).collect();
            vmap(vec![("events", Value::Array(events)), ("active", Value::from(1))])
        } else if code == LIVE_SHOWN_LUA {
            let hidden = self.hidden.borrow();
            let shown = match &args[0] {
                Value::Array(paths) => paths
                    .iter()
                    .map(|path| {
                        let bytes = path.as_slice().unwrap_or_default();
                        Value::from(!hidden.iter().any(|h| h.as_os_str().as_encoded_bytes() == bytes))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            vmap(vec![("shown", Value::Array(shown))])
        } else if code == LIVE_SHOW_LUA {
            let hunks = args[2].as_array().map_or(0, Vec::len) as u64;
            let left = self.left.get();
            vmap(vec![
                ("drawn", Value::from(if left { 0 } else { hunks })),
                ("shown", Value::from(true)),
                ("left", Value::from(left)),
                ("skipped", Value::from(0)),
                ("active", Value::from(1)),
                ("pending", Value::from(0)),
                ("live_only", Value::from(!left)),
            ])
        } else if code == LIVE_CLEAR_LUA {
            vmap(vec![
                ("cleared", Value::from(true)),
                ("active", Value::from(0)),
                ("pending", Value::from(0)),
                ("live_only", Value::from(false)),
            ])
        } else {
            Value::Nil
        };
        let (reply, pending) = Pending::pair();
        if self.hold.borrow().contains(&code) {
            self.held.borrow_mut().push((reply, answer));
        } else {
            reply.send(Ok(answer));
        }
        pending
    }

    fn target(&self) -> Option<u64> {
        self.target.get()
    }
}

fn open_in_editor(id: &str, tab: TabId, turn: u32, path: &str) -> InboundMessage {
    msg(json!({"type":"open_in_editor","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,"line":2}))
}

/// The revert event the review module sends for a one-line hunk of `path` (absolute), with line
/// endings `eol` on both sides.
#[allow(clippy::too_many_arguments)]
fn revert_event(f: &Fixture, n: u32, rel: &str, id: u32, at_line: u32, old: &str, new: &str, eol: &str) -> Value {
    vmap(vec![
        ("kind", Value::from("revert")),
        ("tab", Value::from(f.tab.0)),
        ("session", Value::from(SESSION)),
        ("turn", Value::from(n)),
        ("scope", Value::from("turn")),
        ("path", Value::from(f.file(rel).to_str().unwrap())),
        ("hunk_id", Value::from(id)),
        ("old_start", Value::from(at_line)),
        ("old_len", Value::from(1)),
        ("new_start", Value::from(at_line)),
        ("new_len", Value::from(1)),
        ("at_line", Value::from(at_line)),
        ("old_lines", Value::Array(vec![Value::from(old)])),
        ("new_lines", Value::Array(vec![Value::from(new)])),
        ("old_eols", Value::Array(vec![Value::from(eol)])),
        ("new_eols", Value::Array(vec![Value::from(eol)])),
    ])
}

fn embedded(f: &mut Fixture) {
    f.flow.set_editor_owner(Some(Owner::Embedded));
}

/// Ticks until `done` holds of what the ticks returned, which are gathered and returned.
fn tick_until(f: &mut Fixture, editor: &ScriptedEditor, what: &str, done: impl Fn(&[Out]) -> bool) -> Vec<Out> {
    let mut all = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut at = Instant::now();
    loop {
        // A clock that runs ahead, so the module's events are due on every tick.
        at += Duration::from_secs(1);
        f.pump();
        all.extend(f.flow.tick(&mut f.tabs, editor.rpc(), at));
        if done(&all) {
            return all;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn hunks_arg(args: &[Value]) -> &Value {
    &args[3]
}

#[test]
fn open_in_editor_refuses_paths_outside_the_project_or_with_control_characters() {
    let (mut f, n) = changed_once("open-refused");
    embedded(&mut f);
    let editor = ScriptedEditor::new();
    let outside = f.scratch.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.txt"), b"x\n").unwrap();
    std::os::unix::fs::symlink(&outside, f.file("link")).unwrap();

    // Refused before anything is read.
    for (i, path) in [
        "../x.txt",
        "/etc/passwd",
        "a/../a.txt",
        "",
        "bad\u{1}name.txt",
        "bad\u{7f}.txt",
        "x\ny.txt",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("o{i}");
        let outs = f.handle(editor.rpc(), open_in_editor(&id, f.tab, n, path));
        assert!(result_of(&outs, &id).is_err(), "{path:?} is refused");
    }
    assert_eq!(f.flow.jobs_started(), 0, "no file was read for any of them");

    // A directory that leaves the project is found on the worker, by canonicalizing it.
    let outs = f.run(editor.rpc(), "o-link", open_in_editor("o-link", f.tab, n, "link/x.txt"));
    let why = result_of(&outs, "o-link").unwrap_err();
    assert!(why.contains("not inside the project"), "{why}");
    assert_eq!(editor.asked.borrow().len(), 0, "the editor was never asked");
    f.shut_down();
}

#[test]
fn open_in_editor_sends_one_open_and_show_with_the_turns_hunks() {
    let (mut f, n) = changed_once("open-sends");
    embedded(&mut f);
    let editor = ScriptedEditor::new();
    let outs = f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    assert_eq!(result_of(&outs, "o1"), Ok(Some("opened".to_string())));
    assert!(
        shape(&outs).contains(&"editor_opened".to_string()),
        "the shell is told to show the editor: {:?}",
        shape(&outs)
    );
    assert_eq!(editor.count(REVIEW_LUA), 1, "installed once");
    assert_eq!(editor.count(OPEN_AND_SHOW_LUA), 1, "one open");
    let args = editor.args(OPEN_AND_SHOW_LUA, 0);
    let Value::Binary(path) = &args[0] else {
        panic!("the path travels as bytes: {:?}", args[0])
    };
    assert_eq!(path.as_slice(), f.file("a.txt").to_str().unwrap().as_bytes());
    assert_eq!(args[1], Value::from(2), "the line");
    let Value::Array(hunks) = hunks_arg(&args) else {
        panic!("the hunks travel as a list")
    };
    assert_eq!(hunks.len(), 1);
    let first = hunks[0].as_map().unwrap();
    let get = |name: &str| first.iter().find(|(k, _)| k.as_str() == Some(name)).unwrap().1.clone();
    let lines = |names: &[&[u8]]| Value::Array(names.iter().map(|l| Value::Binary(l.to_vec())).collect());
    assert_eq!(
        get("old_lines"),
        lines(&[b"one", b"two", b"three"]),
        "the hunk's lines, context included"
    );
    assert_eq!(get("new_lines"), lines(&[b"one", b"TWO", b"three"]));
    f.shut_down();
}

#[test]
fn a_binary_or_large_file_opens_without_an_overlay() {
    let mut f = Fixture::new("open-nooverlay");
    embedded(&mut f);
    f.write("blob.bin", b"\x00\x01\x02 old", 0o644);
    f.write("big.txt", b"one\n", 0o644);
    let n = f.turn(|f| {
        f.write("blob.bin", b"\x00\x01\x02 new", 0o644);
        let many: String = (0..2500).map(|i| format!("line {i}\n")).collect();
        f.write("big.txt", many.as_bytes(), 0o644);
    });
    let editor = ScriptedEditor::new();
    *editor.opened.borrow_mut() = vmap(vec![
        ("opened", Value::from(true)),
        ("drawn", Value::from(0)),
        ("skipped", Value::from(0)),
        ("active", Value::from(0)),
    ]);

    let outs = f.run(editor.rpc(), "b1", open_in_editor("b1", f.tab, n, "blob.bin"));
    assert_eq!(
        result_of(&outs, "b1"),
        Ok(Some("opened; no overlay for a binary file".to_string()))
    );
    assert_eq!(hunks_arg(&editor.args(OPEN_AND_SHOW_LUA, 0)), &Value::Nil);

    let outs = f.run(editor.rpc(), "b2", open_in_editor("b2", f.tab, n, "big.txt"));
    assert_eq!(
        result_of(&outs, "b2"),
        Ok(Some("opened; too large for the editor overlay".to_string()))
    );
    assert_eq!(hunks_arg(&editor.args(OPEN_AND_SHOW_LUA, 1)), &Value::Nil);
    f.shut_down();
}

#[test]
fn editor_reverts_reach_the_draft_in_order() {
    let mut f = Fixture::new("editor-reverts");
    embedded(&mut f);
    f.write("a.txt", b"one\ntwo\nthree\nfour\n", 0o644);
    let n = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\nFOUR\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    let panel_hunk = f
        .tabs
        .turn_review()
        .unwrap()
        .exact_hunks_job(SESSION, n, Scope::Turn, "a.txt")
        .run()
        .unwrap()
        .hunks
        .unwrap()
        .remove(0);
    let known = panel_hunk.id;
    let unknown = known + 10;

    editor.queue_event(revert_event(&f, n, "a.txt", unknown, 4, "four", "FOUR", "lf"));
    editor.queue_event(revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"));
    let outs = tick_until(&mut f, &editor, "the reverts", |outs| {
        shape(outs).iter().any(|w| w.starts_with("review_draft"))
    });
    let drafts = shape(&outs)
        .into_iter()
        .filter(|w| w.starts_with("review_draft"))
        .count();
    assert_eq!(drafts, 1, "one push for the batch: {:?}", shape(&outs));
    assert_eq!(shape(&outs)[0], "review_draft:null");

    let records = f.draft().reverts();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].new.hunk.as_ref().map(|h| h.0),
        Some(unknown),
        "the first revert is recorded first"
    );
    assert_eq!(records[1].new.hunk.as_ref().map(|h| h.0), Some(known));
    for record in records {
        assert_eq!(record.new.source, RevertSource::Editor);
        assert!(
            record.new.undo.is_none(),
            "an editor revert is undone with u in the editor"
        );
        assert_eq!(record.new.path, "a.txt");
        assert_eq!(record.new.turn, n);
    }
    assert_eq!(records[0].new.reverted_to, b"four\n");
    assert_eq!(records[0].new.replaced, b"FOUR\n");
    assert_eq!(records[0].new.shape, RevertShape::Lines { from: 4, to: 4 });
    // The known hunk is one the panel's diff gave: its own header is kept. The other is not, so its header is
    // written from its counts.
    assert_eq!(records[1].new.hunk.as_ref().unwrap().1, panel_hunk.header);
    assert_eq!(records[0].new.hunk.as_ref().unwrap().1, "@@ -4 +4 @@");
    f.shut_down();
}

/// `event` with its kind changed: the module's `unrevert` carries the hunk exactly as its `revert` did.
fn with_kind(mut event: Value, kind: &str) -> Value {
    if let Value::Map(entries) = &mut event {
        for (k, v) in entries.iter_mut() {
            if k.as_str() == Some("kind") {
                *v = Value::from(kind);
            }
        }
    }
    event
}

#[test]
fn an_editor_revert_undone_in_the_editor_leaves_the_draft() {
    let mut f = Fixture::new("editor-unrevert");
    embedded(&mut f);
    f.write("a.txt", b"one\ntwo\nthree\nfour\n", 0o644);
    let n = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\nFOUR\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    let panel_hunk = f
        .tabs
        .turn_review()
        .unwrap()
        .exact_hunks_job(SESSION, n, Scope::Turn, "a.txt")
        .run()
        .unwrap()
        .hunks
        .unwrap()
        .remove(0);
    let known = panel_hunk.id;
    let drafts = |outs: &[Out]| shape(outs).iter().filter(|w| w.starts_with("review_draft")).count();

    editor.queue_event(revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"));
    editor.queue_event(revert_event(&f, n, "a.txt", known + 10, 4, "four", "FOUR", "lf"));
    tick_until(&mut f, &editor, "the reverts", |outs| drafts(outs) == 1);
    assert_eq!(f.draft().reverts().len(), 2);

    // `u` in the editor brought the first hunk's text back: its revert leaves the draft, under the
    // header the panel showed, and the panel is told.
    editor.queue_event(with_kind(
        revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"),
        "unrevert",
    ));
    let outs = tick_until(&mut f, &editor, "the un-revert", |outs| drafts(outs) == 1);
    assert_eq!(shape(&outs)[0], "review_draft:null");
    let left = f.draft().reverts();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].new.hunk.as_ref().map(|h| h.0), Some(known + 10));

    // One the draft no longer holds (sent, or never recorded) changes nothing and pushes nothing.
    editor.queue_event(with_kind(
        revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"),
        "unrevert",
    ));
    let mut seen = Vec::new();
    for _ in 0..5 {
        f.pump();
        seen.extend(
            f.flow
                .tick(&mut f.tabs, editor.rpc(), Instant::now() + Duration::from_secs(60)),
        );
    }
    assert_eq!(drafts(&seen), 0, "{:?}", shape(&seen));
    assert_eq!(f.draft().reverts().len(), 1);
    f.shut_down();
}

#[test]
fn a_revert_and_the_undo_the_editor_reports_together_leave_nothing_in_the_draft() {
    let mut f = Fixture::new("editor-undo-after-close");
    embedded(&mut f);
    f.write("a.txt", b"one\ntwo\nthree\nfour\n", 0o644);
    let n = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\nFOUR\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    let known = f
        .tabs
        .turn_review()
        .unwrap()
        .exact_hunks_job(SESSION, n, Scope::Turn, "a.txt")
        .run()
        .unwrap()
        .hunks
        .unwrap()[0]
        .id;
    let drafts = |outs: &[Out]| shape(outs).iter().filter(|w| w.starts_with("review_draft")).count();

    // The overlay closed and the user undid the revert, so the editor, which drew the hunk again on
    // the panel's next `o`, says both in one answer, the revert first, as they happened.
    editor.queue_event(revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"));
    editor.queue_event(with_kind(
        revert_event(&f, n, "a.txt", known, 2, "two", "TWO", "lf"),
        "unrevert",
    ));
    // A second revert, of a hunk that stays reverted, is kept.
    editor.queue_event(revert_event(&f, n, "a.txt", known + 10, 4, "four", "FOUR", "lf"));
    tick_until(&mut f, &editor, "the events", |outs| drafts(outs) >= 1);
    for _ in 0..5 {
        f.pump();
        f.flow
            .tick(&mut f.tabs, editor.rpc(), Instant::now() + Duration::from_secs(60));
    }
    let left = f.draft().reverts();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].new.hunk.as_ref().map(|h| h.0), Some(known + 10));
    f.shut_down();
}

#[test]
fn show_hunk_converts_lf_crlf_and_a_missing_final_newline() {
    let mut f = Fixture::new("show-hunk");
    f.write("lf.txt", b"a\nb\nc\n", 0o644);
    f.write("crlf.txt", b"a\r\nb\r\nc\r\n", 0o644);
    f.write("noeol.txt", b"a\nb\nc", 0o644);
    let n = f.turn(|f| {
        f.write("lf.txt", b"a\nB\nc\n", 0o644);
        f.write("crlf.txt", b"a\r\nB\r\nc\r\n", 0o644);
        f.write("noeol.txt", b"a\nb\nC", 0o644);
    });
    let review = f.tabs.turn_review().unwrap();
    let hunks_of = |rel: &str| {
        review
            .exact_hunks_job(SESSION, n, Scope::Turn, rel)
            .run()
            .unwrap()
            .hunks
            .unwrap()
    };
    use eitri_core::editor_lines::Eol;
    for (rel, old_eol, new_eol, old_bytes, new_bytes) in [
        ("lf.txt", Eol::Lf, Eol::Lf, &b"b\n"[..], &b"B\n"[..]),
        ("crlf.txt", Eol::CrLf, Eol::CrLf, &b"b\r\n"[..], &b"B\r\n"[..]),
        ("noeol.txt", Eol::Missing, Eol::Missing, &b"c"[..], &b"C"[..]),
    ] {
        let exact = hunks_of(rel);
        assert_eq!(exact.len(), 1, "{rel}");
        let shown = show_hunk(&exact[0]);
        let last = |lines: &[eitri_core::editor_lines::BufferLine]| lines.last().unwrap().clone();
        assert_eq!(last(&shown.old_lines).eol, old_eol, "{rel}");
        assert_eq!(last(&shown.new_lines).eol, new_eol, "{rel}");
        for line in shown.old_lines.iter().chain(&shown.new_lines) {
            assert!(
                !line.text.ends_with(b"\n") && !line.text.ends_with(b"\r"),
                "{rel}: no terminator in the text"
            );
        }
        assert_eq!(shown.old_bytes(), exact[0].old_lines.concat(), "{rel}");
        assert_eq!(shown.new_bytes(), exact[0].new_lines.concat(), "{rel}");
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(contains(&shown.old_bytes(), old_bytes), "{rel}");
        assert!(contains(&shown.new_bytes(), new_bytes), "{rel}");
        assert_eq!((shown.old_start, shown.old_len), (exact[0].old_start, exact[0].old_len));
        assert_eq!((shown.new_start, shown.new_len), (exact[0].new_start, exact[0].new_len));
    }
    f.shut_down();
}

#[test]
fn an_editor_revert_records_the_files_own_bytes() {
    let mut f = Fixture::new("editor-crlf");
    embedded(&mut f);
    f.write("w.txt", b"a\r\nb\r\nc\r\n", 0o644);
    let n = f.turn(|f| f.write("w.txt", b"a\r\nB\r\nc\r\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "w.txt"));

    editor.queue_event(revert_event(&f, n, "w.txt", 1, 2, "b", "B", "crlf"));
    tick_until(&mut f, &editor, "the revert", |outs| {
        shape(outs).iter().any(|w| w.starts_with("review_draft"))
    });
    let record = &f.draft().reverts()[0];
    assert_eq!(
        record.new.reverted_to, b"b\r\n",
        "the file's own terminator, not a joined one"
    );
    assert_eq!(record.new.replaced, b"B\r\n");

    // Still the turn's version on disk: the revert is only in the editor.
    let status = |f: &Fixture| check_reverts(&f.project, f.draft(), &Default::default());
    assert_eq!(status(&f)[0].1, RevertStatus::Undone);
    // Written from the editor the way the file had it: on disk.
    f.write("w.txt", b"a\r\nb\r\nc\r\n", 0o644);
    assert_eq!(status(&f)[0].1, RevertStatus::OnDisk);
    f.shut_down();
}

#[test]
fn an_editor_revert_for_a_closed_tab_is_dropped() {
    let (mut f, n) = changed_once("editor-closed");
    embedded(&mut f);
    let spare = f.tabs.open(eitri_core::tab_set::AgentId::Claude);
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    editor.queue_event(revert_event(&f, n, "a.txt", 1, 2, "two", "TWO", "lf"));
    let gone = f.tab;
    let mut closed = f.tabs.remove(gone).expect("the tab was open");
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    let outs = tick_until(&mut f, &editor, "the events to be drained", |_| {
        editor.events.borrow().is_empty()
    });
    assert!(
        !shape(&outs).iter().any(|w| w.starts_with("review_draft")),
        "no draft was pushed for a tab that is gone: {:?}",
        shape(&outs)
    );
    assert!(f.tabs.get(gone).is_none());
    assert_eq!(
        f.tabs.review_draft(spare).unwrap().reverts().len(),
        0,
        "another tab's draft is untouched"
    );
    f.tab = spare;
    f.shut_down();
}

#[test]
fn showing_another_turn_replaces_the_overlay() {
    let mut f = Fixture::new("redraw");
    embedded(&mut f);
    f.write("a.txt", b"one\ntwo\nthree\n", 0o644);
    let first = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\n", 0o644));
    let second = f.turn(|f| f.write("a.txt", b"one\nTWO\nTHREE\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, first, "a.txt"));
    assert_eq!(editor.count(SHOW_LUA), 0);

    // The panel looking at the same turn again changes nothing in the editor.
    f.flow.panel_shows(&f.tabs, f.tab, first, Scope::Turn, "a.txt");
    for _ in 0..40 {
        f.pump();
        f.flow.tick(&mut f.tabs, editor.rpc(), Instant::now());
        std::thread::sleep(Duration::from_millis(5));
    }
    f.settle_file_jobs(editor.rpc());
    assert_eq!(editor.count(SHOW_LUA), 0, "the same turn is not drawn twice");

    // Another turn's diff for that file replaces what is drawn.
    f.flow.panel_shows(&f.tabs, f.tab, second, Scope::Turn, "a.txt");
    let deadline = Instant::now() + Duration::from_secs(15);
    while editor.count(SHOW_LUA) == 0 {
        assert!(Instant::now() < deadline, "the overlay was never replaced");
        f.pump();
        f.flow.tick(&mut f.tabs, editor.rpc(), Instant::now());
        std::thread::sleep(Duration::from_millis(5));
    }
    let args = editor.args(SHOW_LUA, 0);
    let meta = args[1].as_map().unwrap();
    let turn = meta.iter().find(|(k, _)| k.as_str() == Some("turn")).unwrap().1.clone();
    assert_eq!(turn, Value::from(second));
    f.shut_down();
}

#[test]
fn a_file_the_editor_does_not_draw_is_not_redrawn() {
    let (mut f, n) = changed_once("no-redraw");
    embedded(&mut f);
    let editor = ScriptedEditor::new();
    f.flow.panel_shows(&f.tabs, f.tab, n, Scope::Turn, "a.txt");
    assert_eq!(f.flow.jobs_started(), 0, "nothing is drawn, so nothing is read");
    assert_eq!(editor.asked.borrow().len(), 0);
    f.shut_down();
}

#[test]
fn cancel_drafts_forgets_the_install() {
    let (mut f, n) = changed_once("lost");
    embedded(&mut f);
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    assert_eq!(editor.count(REVIEW_LUA), 1);
    assert!(f.flow.editor_lost().is_empty(), "nothing was waiting");
    f.run(editor.rpc(), "o2", open_in_editor("o2", f.tab, n, "a.txt"));
    assert_eq!(editor.count(REVIEW_LUA), 2, "the next call installs again");
    f.shut_down();
}

#[test]
fn an_open_waiting_for_an_editor_that_is_lost_is_told_so() {
    let (mut f, n) = changed_once("lost-waiting");
    embedded(&mut f);
    let editor = FakeEditor::holding();
    assert!(f
        .handle(editor.rpc(), open_in_editor("o1", f.tab, n, "a.txt"))
        .is_empty());
    let mut waiting = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while editor.asked() == 0 {
        assert!(Instant::now() < deadline);
        waiting.extend(f.tick(editor.rpc()));
        std::thread::sleep(Duration::from_millis(5));
    }
    let outs = f.flow.editor_lost();
    assert!(result_of(&outs, "o1").unwrap_err().contains("went away"));
    f.shut_down();
}

#[test]
fn an_open_still_reading_its_file_never_reaches_an_editor_that_replaced_the_one_asked() {
    let (mut f, n) = changed_once("owner-mid-read");
    f.flow.set_editor_owner(Some(Owner::Companion { channel: 4 }));
    // The old editor never answers, so the open is either still being read or waiting on it: both
    // must end in a refusal when the owner changes, and neither may reach the new editor.
    let old = FakeEditor::holding();
    let mut outs = f.handle(old.rpc(), open_in_editor("o1", f.tab, n, "a.txt"));
    f.flow.set_editor_owner(Some(Owner::Companion { channel: 5 }));
    let new = ScriptedEditor::new();
    for _ in 0..40 {
        outs.extend(f.tick(new.rpc()));
        std::thread::sleep(Duration::from_millis(5));
    }
    outs.extend(f.settle_file_jobs(new.rpc()));
    let why = result_of(&outs, "o1").unwrap_err();
    assert!(why.contains("editor changed"), "{why}");
    assert_eq!(
        new.count(OPEN_AND_SHOW_LUA),
        0,
        "the new editor was never asked to show it"
    );
    assert_eq!(new.count(REVIEW_LUA), 0, "nor to install anything for it");
    f.shut_down();
}

#[test]
fn the_newest_panel_request_for_a_file_is_the_one_drawn() {
    let mut f = Fixture::new("redraw-latest");
    embedded(&mut f);
    f.write("a.txt", b"one\ntwo\nthree\n", 0o644);
    let first = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\n", 0o644));
    let second = f.turn(|f| f.write("a.txt", b"one\nTWO\nTHREE\n", 0o644));
    let third = f.turn(|f| f.write("a.txt", b"ONE\nTWO\nTHREE\n", 0o644));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, first, "a.txt"));

    // The panel moves on to a third turn before the second one's file was read: the second is
    // never drawn, whichever worker finishes first.
    f.flow.panel_shows(&f.tabs, f.tab, second, Scope::Turn, "a.txt");
    f.flow.panel_shows(&f.tabs, f.tab, third, Scope::Turn, "a.txt");
    for _ in 0..60 {
        f.pump();
        f.flow.tick(&mut f.tabs, editor.rpc(), Instant::now());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(editor.count(SHOW_LUA), 1, "one redraw, for the turn the panel is on");
    let args = editor.args(SHOW_LUA, 0);
    let meta = args[1].as_map().unwrap();
    let turn = meta.iter().find(|(k, _)| k.as_str() == Some("turn")).unwrap().1.clone();
    assert_eq!(turn, Value::from(third));
    f.shut_down();
}

#[test]
fn a_new_owner_is_a_new_editor() {
    let (mut f, n) = changed_once("owner");
    f.flow.set_editor_owner(Some(Owner::Companion { channel: 4 }));
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    f.flow.set_editor_owner(Some(Owner::Companion { channel: 5 }));
    f.run(editor.rpc(), "o2", open_in_editor("o2", f.tab, n, "a.txt"));
    assert_eq!(editor.count(REVIEW_LUA), 2, "each owner installs its own");
    let owners: Vec<_> = (0..2)
        .map(|i| editor.args(REVIEW_LUA, i)[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(owners, ["companion:4", "companion:5"]);
    f.flow.set_editor_owner(None);
    let outs = f.handle(editor.rpc(), open_in_editor("o3", f.tab, n, "a.txt"));
    assert!(result_of(&outs, "o3").is_err(), "no owner, no editor to show it in");
    f.shut_down();
}

#[test]
fn review_editor_never_types_keys() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = [root.join("src/review_editor.rs"), root.join("src/turn_review/flow.rs")];
    let shell_sources = eitri_core::source_scan::product_sources();
    let shell_editor = eitri_core::source_scan::file_with(&shell_sources, "struct ReviewEditor");
    // Spelled in pieces so this file never contains what it looks for.
    let needles = [["send", "_keys"].concat(), ["input", "_keys"].concat()];
    let mut texts: Vec<(String, String)> = files
        .iter()
        .map(|file| {
            let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            (file.display().to_string(), text)
        })
        .collect();
    texts.push((shell_editor.path.clone(), shell_editor.text.clone()));
    for (name, text) in &texts {
        for needle in &needles {
            assert!(!text.contains(needle.as_str()), "{name} mentions {needle}");
        }
    }
}

// ---- the project's write slot ----------------------------------------------------------------------

const HOLD_DIR: &str = "EITRI_FLOW_SLOT_HOLD_DIR";
const HOLD_SIGNALS: &str = "EITRI_FLOW_SLOT_HOLD_SIGNALS";

/// The child that stands for another window writing: it takes the review directory's write slot
/// exclusively, says so with a `held` file and keeps it until a `release` file appears. Does nothing
/// unless the parent set its directories.
#[test]
#[ignore = "run by the write slot tests"]
fn write_slot_holder_child() {
    let (Some(dir), Some(signals)) = (std::env::var_os(HOLD_DIR), std::env::var_os(HOLD_SIGNALS)) else {
        return;
    };
    let signals = PathBuf::from(signals);
    let slot = WriteSlot::open(Path::new(&dir)).unwrap();
    let hold = slot.try_exclusive().expect("the slot is free when the holder starts");
    std::fs::write(signals.join("held"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !signals.join("release").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(hold);
}

/// Another process holding the slot exclusively until dropped.
struct SlotHolder {
    child: std::process::Child,
    signals: PathBuf,
}

impl SlotHolder {
    fn start(f: &Fixture) -> SlotHolder {
        let signals = f.scratch.0.join("slot-signals");
        std::fs::create_dir_all(&signals).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write_slot_holder_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HOLD_DIR, &f.review_dir)
            .env(HOLD_SIGNALS, &signals)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let holder = SlotHolder { child, signals };
        until("the other process to hold the slot", || {
            holder.signals.join("held").exists()
        });
        holder
    }
}

impl Drop for SlotHolder {
    fn drop(&mut self) {
        let _ = std::fs::write(self.signals.join("release"), b"");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn every_disk_job_takes_the_slot_and_refuses_with_the_busy_note_while_another_process_holds_it() {
    let _one = one_crash_at_a_time();
    let mut f = Fixture::new("slot-busy");
    f.write("a.txt", BASE, 0o644);
    f.write("b.txt", BASE, 0o644);
    let n = f.turn(|f| {
        f.write("a.txt", CHANGED, 0o644);
        f.write("b.txt", CHANGED, 0o644);
    });
    let editor = FakeEditor::answering(clear_buffer());
    // A revert to undo, and an interrupted one to recover, made before the other window comes.
    let outs = f.run(editor.rpc(), "r0", revert_file("r0", f.tab, n, "a.txt"));
    assert_eq!(result_of(&outs, "r0"), Ok(None));
    f.write("c.txt", b"the original\n", 0o644);
    let entry = crash_mid_write(&f, "c.txt");
    assert_eq!(first_offer(&mut f), vec![entry.clone()]);
    let half = f.read("c.txt");

    let holder = SlotHolder::start(&f);
    let note = SlotBusy::Writing.to_string();
    assert!(note.contains("another write"), "the note says what holds it: {note}");
    for (id, message) in [
        ("r1", revert_file("r1", f.tab, n, "b.txt")),
        ("u1", undo("u1", f.tab)),
        ("v1", recover("v1", &entry, "restore")),
        ("s1", send("s1", f.tab, None)),
    ] {
        let outs = f.run(editor.rpc(), id, message);
        assert_eq!(result_of(&outs, id), Err(note.clone()), "{id}");
    }
    assert_eq!(f.read("a.txt"), BASE, "the undo wrote nothing");
    assert_eq!(f.read("b.txt"), CHANGED, "the revert wrote nothing");
    assert_eq!(f.read("c.txt"), half, "the recovery wrote nothing");
    assert_eq!(pending_ids(&f), vec![entry.clone()], "the entry is still pending");
    assert_eq!(f.draft().reverts().len(), 1);
    assert!(!f.draft().reverts()[0].undone, "the undo was not recorded");

    // The other window lets go: the same requests go through.
    drop(holder);
    let outs = f.run(editor.rpc(), "r2", revert_file("r2", f.tab, n, "b.txt"));
    assert_eq!(result_of(&outs, "r2"), Ok(None));
    assert_eq!(f.read("b.txt"), BASE);
    let outs = f.run(None, "s2", send("s2", f.tab, None));
    preview_of(&outs);
    f.shut_down();
}

#[test]
fn a_send_in_a_window_without_presence_still_takes_the_slot() {
    let mut f = Fixture::without_presence("slot-no-presence");
    assert!(f.tabs.presence_guard().is_none());
    f.write("a.txt", BASE, 0o644);
    let n = f.turn(|f| f.write("a.txt", CHANGED, 0o644));
    f.run(None, "c1", comment("c1", f.tab, n, "a.txt", "why this?"));

    let holder = SlotHolder::start(&f);
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    assert_eq!(result_of(&outs, "s1"), Err(SlotBusy::Writing.to_string()));
    assert_eq!(f.draft().comments().len(), 1, "the draft is kept");

    drop(holder);
    let outs = f.run(None, "s2", send("s2", f.tab, None));
    preview_of(&outs);
    f.shut_down();
}

// ---- window 2 --------------------------------------------------------------------------------------

/// Makes the fixture's flow window 2's: every operation it sends to window 1 is recorded, and a
/// buffer question is answered with `buffer` (`None`: not linked).
fn as_window_2(f: &mut Fixture, buffer: Option<Value>) -> Rc<RefCell<Vec<EditorOp>>> {
    let sent: Rc<RefCell<Vec<EditorOp>>> = Rc::default();
    let ops = sent.clone();
    let overlay = RemoteOverlay::new(
        &f.project,
        Box::new(move |op| {
            ops.borrow_mut().push(op);
            ops.borrow().len() as u64
        }),
    );
    let asks = sent.clone();
    let ask = RemoteAsk::new(
        &f.project,
        Box::new(move |op| {
            asks.borrow_mut().push(op);
            buffer.clone().map(|value| {
                let (answer, pending) = Pending::pair();
                answer.send(Ok(value));
                pending
            })
        }),
    );
    f.flow.link_to_window_1(overlay, ask);
    sent
}

/// Ticks window 2's flow, which has no editor of its own, until `done` holds of what it sent.
fn tick_until_sent(f: &mut Fixture, sent: &RefCell<Vec<EditorOp>>, what: &str, done: impl Fn(&[EditorOp]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(sent.borrow().as_slice()) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        f.tick(None);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_panel_moving_to_another_turn_of_a_drawn_file_redraws_it_from_window_2() {
    let mut f = Fixture::new("window-2-redraw");
    f.write("a.txt", b"one\ntwo\nthree\n", 0o644);
    let first = f.turn(|f| f.write("a.txt", b"one\nTWO\nthree\n", 0o644));
    let second = f.turn(|f| f.write("a.txt", b"one\nTWO\nTHREE\n", 0o644));
    let sent = as_window_2(&mut f, None);

    // The open goes to window 1 as an operation, and is answered when window 1 answers it.
    let outs = f.handle(None, open_in_editor("o1", f.tab, first, "a.txt"));
    assert!(!answered(&outs, "o1"), "window 2 has no editor, but window 1 does");
    tick_until_sent(&mut f, &sent, "the open", |ops| !ops.is_empty());
    assert!(
        matches!(&sent.borrow()[0], EditorOp::ReviewOpenAndShow { path, hunks: Some(h), .. } if path == "a.txt" && h.len() == 1),
        "{:?}",
        sent.borrow()
    );
    f.flow.remote_overlay().unwrap().answered(1, Ok("opened".to_owned()));
    let outs = f.drive(None, "o1");
    assert_eq!(result_of(&outs, "o1"), Ok(Some("opened".to_string())));

    // Until window 1 says the file is drawn, the panel moving on redraws nothing.
    let jobs = f.flow.jobs_started();
    f.flow.panel_shows(&f.tabs, f.tab, second, Scope::Turn, "a.txt");
    assert_eq!(
        f.flow.jobs_started(),
        jobs,
        "nothing is known to be drawn, so nothing is read"
    );

    f.flow.remote_overlay().unwrap().review_drawn("a.txt", Drawn::Drawn);
    f.flow.panel_shows(&f.tabs, f.tab, second, Scope::Turn, "a.txt");
    tick_until_sent(&mut f, &sent, "the redraw", |ops| ops.len() > 1);
    let ops = sent.borrow();
    let EditorOp::ReviewShow { path, meta, hunks } = &ops[1] else {
        panic!("a redraw is a show: {:?}", ops[1]);
    };
    assert_eq!(path, "a.txt");
    assert_eq!(meta.turn, second, "the turn the panel moved to");
    assert_eq!(meta.session, SESSION);
    assert_eq!(hunks.len(), 1);
    drop(ops);
    f.shut_down();
}

#[test]
fn a_review_send_asks_unsaved_editor_reverts_through_remote_ask() {
    let (mut f, n) = changed_once("window-2-send");
    f.write("e.txt", b"old1\nold2\n", 0o644);
    f.tabs.review_draft_mut(f.tab).unwrap().record_revert(NewRevert {
        turn: n,
        path: "e.txt".into(),
        hunk: None,
        shape: RevertShape::Lines { from: 1, to: 2 },
        source: RevertSource::Editor,
        at_line: 1,
        reverted_to: b"alpha\nbeta\n".to_vec(),
        replaced: b"old1\nold2\n".to_vec(),
        undo: None,
    });
    let sent = as_window_2(&mut f, Some(buffer_with(&["alpha", "beta"], "unix", true, true)));
    let outs = f.run(None, "s1", send("s1", f.tab, None));
    let preview = preview_of(&outs);
    assert_eq!(
        preview["notOnDisk"][0]["why"], "only in the editor, not saved",
        "window 1's editor vouched for it: {preview}"
    );
    assert_eq!(
        *sent.borrow(),
        vec![EditorOp::BufferState {
            path: "e.txt".to_owned(),
            lines: Some((1, 2)),
        }],
        "asked of window 1, by the path from the project root and the revert's own lines"
    );
    f.shut_down();
}

#[test]
fn an_editor_revert_from_window_1_finds_its_tab_by_session_not_by_number() {
    let (mut f, n) = changed_once("window-2-events");
    let _sent = as_window_2(&mut f, None);
    // Window 1 numbered the tab otherwise: the number in the event is not this window's.
    let mut event = parse_revert_event(&revert_event(&f, n, "a.txt", 1, 2, "two", "TWO", "lf")).unwrap();
    event.meta.tab = f.tab.0 + 40;
    f.flow
        .remote_overlay()
        .unwrap()
        .review_events(SESSION, vec![editor_event_to_wire(EventKind::Revert, &event)]);
    let outs = f.tick(None);
    assert!(
        shape(&outs).iter().any(|w| w.starts_with("review_draft")),
        "the draft is pushed: {:?}",
        shape(&outs)
    );
    assert_eq!(
        f.draft().reverts().len(),
        1,
        "recorded in the tab that holds the session"
    );
    f.shut_down();
}

// ---- live drawings ---------------------------------------------------------------------------------

/// The agent's edits drawn over the file as they land, and again from the snapshots when the turn
/// ends. Every clock here is frozen unless a test moves it, so no limit runs out on a slow machine.
mod live_review {
    use super::*;
    use eitri_core::turn_review::flow::LiveAsk;
    use eitri_core::turn_review::live_contest::TurnKey;
    use std::collections::BTreeMap;

    /// A window with review.live on and its editor embedded.
    fn live(name: &str) -> Fixture {
        let mut f = Fixture::new(name);
        f.tabs.set_live_edits(true);
        embedded(&mut f);
        f
    }

    /// A second tab of the window, holding `session`.
    fn second_tab(f: &mut Fixture, session: &str) -> (TabId, Arc<RecordingProvider>) {
        let tab = f.tabs.open(eitri_core::tab_set::AgentId::Claude);
        let (provider, backend) = live_backend(&f.project);
        f.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        provider.open_session(session, &f.project);
        until("the second session", || {
            f.pump();
            f.tabs.get(tab).unwrap().provider_session_id().as_deref() == Some(session)
        });
        (tab, provider)
    }

    fn record(f: &Fixture, session: &str, turn_id: &str) -> Option<eitri_core::turn_review::TurnRecord> {
        f.tabs
            .turn_review()
            .unwrap()
            .turns(session)
            .into_iter()
            .find(|r| r.turn_id == turn_id)
    }

    /// Starts `turn_id` of `session` and waits for its base.
    fn begin(f: &mut Fixture, provider: &RecordingProvider, session: &str, turn_id: &str) -> u32 {
        provider.queue(AgentDomainEvent::TurnStarted {
            turn_id: turn_id.to_owned(),
        });
        until("the turn's base", || {
            f.pump();
            record(f, session, turn_id).is_some_and(|r| matches!(r.base, Snap::Taken { .. }))
        });
        record(f, session, turn_id).unwrap().n
    }

    /// Ends `turn_id` of `session` and waits for its end snapshot.
    fn finish(f: &mut Fixture, provider: &RecordingProvider, session: &str, turn_id: &str) {
        provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.to_owned(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        until("the turn's end", || {
            f.pump();
            record(f, session, turn_id).is_some_and(|r| matches!(r.end, Snap::Taken { .. }))
        });
    }

    /// Writes `bytes` to `rel` and reports it as `tab`'s completed `Edit` in `turn_id`; waits until
    /// the pump has read the call's end.
    fn edit_by(f: &mut Fixture, tab: TabId, provider: &RecordingProvider, turn_id: &str, rel: &str, bytes: &[u8]) {
        f.write(rel, bytes, 0o644);
        let id = format!("edit-{}", uuid::Uuid::new_v4().simple());
        provider.queue_all(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: turn_id.to_owned(),
                tool_use_id: id.clone(),
                name: "Edit".to_owned(),
                input: json!({"file_path": f.file(rel).to_str().unwrap()}),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: turn_id.to_owned(),
                tool_use_id: id.clone(),
                content: json!("ok"),
                is_error: false,
            },
        ]);
        until("the edit's end", || {
            f.pump();
            f.tabs.get(tab).and_then(|t| t.live()).is_some_and(|b| {
                b.projection()
                    .tool_calls
                    .iter()
                    .any(|c| c.tool_use_id == id && c.result.is_some())
            })
        });
    }

    /// The fixture's own tab's edit in its running turn.
    fn edit(f: &mut Fixture, rel: &str, bytes: &[u8]) {
        let (tab, provider, turn) = (f.tab, f.provider.clone(), format!("t{}", f.turns));
        edit_by(f, tab, &provider, &turn, rel, bytes);
    }

    /// One pump, one live tick and one tick of the flow, all at `at`.
    fn step(f: &mut Fixture, rpc: Option<&dyn EditorRpc>, at: Instant) -> Vec<Out> {
        f.pump();
        f.flow.live_tick(&mut f.tabs, rpc, at);
        f.flow.tick(&mut f.tabs, rpc, at)
    }

    /// Steps at `at` until `done` holds; returns what the flow's ticks gave meanwhile.
    fn live_until(
        f: &mut Fixture,
        editor: &ScriptedEditor,
        at: Instant,
        what: &str,
        done: impl Fn(&Fixture, &ScriptedEditor) -> bool,
    ) -> Vec<Out> {
        let mut all = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            all.extend(step(f, editor.rpc(), at));
            if done(f, editor) {
                return all;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Steps at `at` until the window has no live work left.
    fn live_settled(f: &mut Fixture, editor: &ScriptedEditor, at: Instant) {
        live_until(f, editor, at, "the live drawings to settle", |f, _| {
            f.flow.live_is_idle()
        });
        // And a few more, so anything those last steps queued has gone out and been answered.
        for _ in 0..10 {
            step(f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn bytes_of(value: &Value) -> Vec<u8> {
        value.as_slice().expect("a path travels as bytes").to_vec()
    }

    fn path_bytes(path: &Path) -> Vec<u8> {
        path.as_os_str().as_encoded_bytes().to_vec()
    }

    fn meta_field(args: &[Value], name: &str) -> Value {
        args[1]
            .as_map()
            .expect("the meta is a map")
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .unwrap_or_else(|| panic!("the meta has no {name}"))
            .1
            .clone()
    }

    /// Every live draw sent, as (path, turn id, running).
    fn live_shows(editor: &ScriptedEditor) -> Vec<(Vec<u8>, String, bool)> {
        (0..editor.count(LIVE_SHOW_LUA))
            .map(|i| {
                let args = editor.args(LIVE_SHOW_LUA, i);
                (
                    bytes_of(&args[0]),
                    meta_field(&args, "turn_id").as_str().unwrap().to_owned(),
                    meta_field(&args, "running").as_bool().unwrap(),
                )
            })
            .collect()
    }

    fn live_clears(editor: &ScriptedEditor) -> Vec<Vec<u8>> {
        (0..editor.count(LIVE_CLEAR_LUA))
            .map(|i| bytes_of(&editor.args(LIVE_CLEAR_LUA, i)[0]))
            .collect()
    }

    fn panel_hunks(f: &Fixture, session: &str, turn: u32, rel: &str) -> Vec<eitri_core::turn_review::ExactHunk> {
        f.tabs
            .turn_review()
            .unwrap()
            .exact_hunks_job(session, turn, Scope::Turn, rel)
            .run()
            .unwrap()
            .hunks
            .unwrap()
    }

    const FOUR: &[u8] = b"one\ntwo\nthree\nfour\n";
    const FOUR_EDITED: &[u8] = b"one\nTWO\nthree\nfour\n";

    #[test]
    fn an_edit_of_a_shown_file_is_drawn_live() {
        let mut f = live("live-drawn");
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the live drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });

        assert_eq!(editor.count(LIVE_SHOWN_LUA), 1, "asked once whether the file is shown");
        let asked = editor.args(LIVE_SHOWN_LUA, 0);
        let asked: Vec<Vec<u8>> = asked[0].as_array().unwrap().iter().map(bytes_of).collect();
        assert_eq!(asked, vec![path_bytes(&f.file("a.txt"))]);
        let args = editor.args(LIVE_SHOW_LUA, 0);
        assert_eq!(bytes_of(&args[0]), path_bytes(&f.file("a.txt")));
        assert_eq!(meta_field(&args, "running"), Value::from(true), "the turn still runs");
        assert_eq!(meta_field(&args, "session"), Value::from(SESSION));
        assert_eq!(meta_field(&args, "turn_id"), Value::from("t1"));
        assert_eq!(
            meta_field(&args, "read_lines"),
            Value::from(4),
            "the lines the disk read held"
        );
        let hunks = args[2].as_array().unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(editor.count(REVIEW_LUA), 1, "the module is installed for the draw");
        f.end_turn();
        f.shut_down();
    }

    /// An edit whose tool named its file through a link and `..` is drawn on the file the disk names,
    /// which only the disk can say (`alias/..` is where the link leads, not the root).
    #[test]
    fn an_edit_named_through_a_link_and_dotdot_is_drawn_on_the_file_the_disk_names() {
        let mut f = live("live-dotdot");
        std::fs::create_dir_all(f.file("other/sub")).unwrap();
        std::os::unix::fs::symlink(f.file("other/sub"), f.file("alias")).unwrap();
        f.write("other/a.txt", FOUR, 0o644);
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        let (tab, provider, turn) = (f.tab, f.provider.clone(), format!("t{}", f.turns));
        f.write("other/a.txt", FOUR_EDITED, 0o644);
        let id = format!("edit-{}", uuid::Uuid::new_v4().simple());
        let named = f.file("alias").join("../a.txt");
        provider.queue_all(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: turn.clone(),
                tool_use_id: id.clone(),
                name: "Edit".to_owned(),
                input: json!({"file_path": named.to_str().unwrap()}),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: turn.clone(),
                tool_use_id: id.clone(),
                content: json!("ok"),
                is_error: false,
            },
        ]);
        until("the edit's end", || {
            f.pump();
            f.tabs.get(tab).and_then(|t| t.live()).is_some_and(|b| {
                b.projection()
                    .tool_calls
                    .iter()
                    .any(|c| c.tool_use_id == id && c.result.is_some())
            })
        });
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the live drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        let asked = editor.args(LIVE_SHOWN_LUA, 0);
        let asked: Vec<Vec<u8>> = asked[0].as_array().unwrap().iter().map(bytes_of).collect();
        assert_eq!(asked, vec![path_bytes(&f.file("other/a.txt"))]);
        assert_eq!(
            bytes_of(&editor.args(LIVE_SHOW_LUA, 0)[0]),
            path_bytes(&f.file("other/a.txt"))
        );
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn an_edit_of_a_file_not_shown_runs_no_job() {
        let mut f = live("live-hidden");
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        editor.hidden.borrow_mut().push(f.file("a.txt"));
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the shown question", |_, e| {
            e.count(LIVE_SHOWN_LUA) == 1
        });
        for _ in 0..20 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(f.flow.jobs_started(), 0, "no git work for a file nobody sees");

        // Asked once more a little after the edit, in case following opened it meanwhile; then never.
        let later = at + Duration::from_secs(2);
        live_until(&mut f, &editor, later, "the second question", |_, e| {
            e.count(LIVE_SHOWN_LUA) == 2
        });
        for _ in 0..20 {
            step(&mut f, editor.rpc(), later);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(editor.count(LIVE_SHOWN_LUA), 2);
        assert_eq!(f.flow.jobs_started(), 0);
        assert_eq!(editor.count(LIVE_SHOW_LUA), 0);
        assert_eq!(editor.count(REVIEW_LUA), 0, "nothing drawn, so nothing installed");
        f.end_turn();
        f.shut_down();
    }

    /// A turn that drew `a.txt` live and settled; returns its number.
    fn settled_once(f: &mut Fixture, editor: &ScriptedEditor, at: Instant) -> u32 {
        f.write("a.txt", FOUR, 0o644);
        let n = f.start_turn();
        edit(f, "a.txt", FOUR_EDITED);
        live_until(f, editor, at, "the live drawing", |_, e| e.count(LIVE_SHOW_LUA) == 1);
        f.end_turn();
        live_until(f, editor, at, "the settle's drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 2
        });
        n
    }

    #[test]
    fn the_settle_redraws_from_the_snapshots_with_the_panels_headers() {
        let mut f = live("live-settle");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let n = settled_once(&mut f, &editor, at);
        let args = editor.args(LIVE_SHOW_LUA, 1);
        assert_eq!(meta_field(&args, "running"), Value::from(false), "the turn has ended");
        assert_eq!(meta_field(&args, "turn"), Value::from(n));
        assert_eq!(
            meta_field(&args, "read_lines"),
            Value::from(4),
            "the end snapshot's lines"
        );
        let panel = panel_hunks(&f, SESSION, n, "a.txt");
        let drawn = args[2].as_array().unwrap();
        assert_eq!(drawn.len(), panel.len(), "the panel's hunks, from the snapshots");
        let id_of = |hunk: &Value| {
            hunk.as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some("id"))
                .unwrap()
                .1
                .clone()
        };
        let ids: Vec<Value> = drawn.iter().map(id_of).collect();
        let panel_ids: Vec<Value> = panel.iter().map(|h| Value::from(h.id)).collect();
        assert_eq!(ids, panel_ids);
        f.shut_down();
    }

    #[test]
    fn an_editor_revert_after_the_settle_lands_under_the_panels_header() {
        let mut f = live("live-settle-revert");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let n = settled_once(&mut f, &editor, at);
        // Let the settle's answer reach the flow.
        for _ in 0..10 {
            step(&mut f, editor.rpc(), at);
        }
        let panel = panel_hunks(&f, SESSION, n, "a.txt").remove(0);
        assert_ne!(
            panel.header, "@@ -2 +2 @@",
            "the panel's header is not the one counts would give"
        );
        editor.queue_event(revert_event(&f, n, "a.txt", panel.id, 2, "two", "TWO", "lf"));
        tick_until(&mut f, &editor, "the revert", |outs| {
            shape(outs).iter().any(|w| w.starts_with("review_draft"))
        });
        let records = f.draft().reverts();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].new.hunk.as_ref().unwrap().1, panel.header);
        f.shut_down();
    }

    #[test]
    fn an_o_overlays_revert_keeps_its_header_after_a_settle_of_another_turn() {
        let mut f = live("live-keeps-o");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write(
            "a.txt",
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
            0o644,
        );
        let first = f.turn(|f| {
            f.write(
                "a.txt",
                b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
                0o644,
            )
        });
        f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, first, "a.txt"));
        let panel = panel_hunks(&f, SESSION, first, "a.txt").remove(0);

        // The `o` drawing is up, so every live drawing of the next turn leaves it.
        editor.left.set(true);
        let second = f.start_turn();
        edit(
            &mut f,
            "a.txt",
            b"one\nTWO\nthree\nfour\nfive\nsix\nseven\nEIGHT\nnine\n",
        );
        live_until(&mut f, &editor, at, "the live drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        f.end_turn();
        live_until(&mut f, &editor, at, "the settle's drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 2
        });
        live_settled(&mut f, &editor, at);
        let other = panel_hunks(&f, SESSION, second, "a.txt").remove(0);
        assert_ne!(other.header, panel.header, "the two turns' hunks differ");

        editor.queue_event(revert_event(&f, first, "a.txt", panel.id, 2, "two", "TWO", "lf"));
        tick_until(&mut f, &editor, "the revert", |outs| {
            shape(outs).iter().any(|w| w.starts_with("review_draft"))
        });
        let records = f.draft().reverts();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].new.hunk.as_ref().unwrap().1,
            panel.header,
            "recorded under the header of the turn `o` drew"
        );
        f.shut_down();
    }

    #[test]
    fn a_later_turn_of_another_tab_draws_after_both_ended() {
        let mut f = live("live-later-turn");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let (other, provider) = second_tab(&mut f, "other-session");
        let (me, mine) = (f.tab, f.provider.clone());
        begin(&mut f, &mine, SESSION, "a1");
        begin(&mut f, &provider, "other-session", "b1");
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nthree\nfour\n");
        live_until(&mut f, &editor, at, "the first drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        edit_by(&mut f, other, &provider, "b1", "a.txt", b"one\nTWO\nTHREE\nfour\n");
        live_until(&mut f, &editor, at, "the contest's clear", |_, e| {
            e.count(LIVE_CLEAR_LUA) >= 1
        });
        finish(&mut f, &mine, SESSION, "a1");
        finish(&mut f, &provider, "other-session", "b1");
        live_settled(&mut f, &editor, at);
        let before = editor.count(LIVE_SHOW_LUA);

        begin(&mut f, &provider, "other-session", "b2");
        edit_by(&mut f, other, &provider, "b2", "a.txt", b"one\nTWO\nTHREE\nFOUR\n");
        live_until(&mut f, &editor, at, "the later turn's drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) > before
        });
        let (path, turn, running) = live_shows(&editor).pop().unwrap();
        assert_eq!(path, path_bytes(&f.file("a.txt")));
        assert_eq!(turn, "b2", "a turn neither of the two ends the contest");
        assert!(running);
        finish(&mut f, &provider, "other-session", "b2");
        f.shut_down();
    }

    #[test]
    fn a_contested_path_is_cleared_at_settle() {
        let mut f = live("live-contested");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let (other, provider) = second_tab(&mut f, "other-session");
        let (me, mine) = (f.tab, f.provider.clone());
        begin(&mut f, &mine, SESSION, "a1");
        begin(&mut f, &provider, "other-session", "b1");
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nthree\nfour\n");
        live_until(&mut f, &editor, at, "the first drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });

        // The other tab's turn, running at the same time, edits the same file: neither draws it.
        edit_by(&mut f, other, &provider, "b1", "a.txt", b"one\nTWO\nTHREE\nfour\n");
        live_until(&mut f, &editor, at, "the contest's clear", |_, e| {
            e.count(LIVE_CLEAR_LUA) >= 1
        });
        finish(&mut f, &mine, SESSION, "a1");
        // The other turn still runs, so its live work goes on: the first turn settles meanwhile.
        let first = TurnKey {
            session: SESSION.to_owned(),
            turn_id: "a1".to_owned(),
        };
        live_until(&mut f, &editor, at, "the first turn's settle", |f, _| {
            !f.flow.live_follows(&first)
        });
        edit_by(&mut f, other, &provider, "b1", "a.txt", b"one\nTWO\nTHREE\nFOUR\n");
        finish(&mut f, &provider, "other-session", "b1");
        live_settled(&mut f, &editor, at);

        let shows = live_shows(&editor);
        assert_eq!(
            shows,
            vec![(path_bytes(&f.file("a.txt")), "a1".to_owned(), true)],
            "only the first turn's drawing, before the file was contested"
        );
        assert!(
            live_clears(&editor).iter().all(|p| *p == path_bytes(&f.file("a.txt"))),
            "only the contested file is cleared"
        );
        assert!(f.flow.jobs_started() > 0, "the settles ran, and drew nothing");
        f.shut_down();
    }

    /// A turn that changed `a.txt` and ended, then a running turn drawing its own edit of it live.
    fn drawn_while_running(f: &mut Fixture, editor: &ScriptedEditor, at: Instant) -> (u32, u32) {
        f.write(
            "a.txt",
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
            0o644,
        );
        let first = f.turn(|f| {
            f.write(
                "a.txt",
                b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
                0o644,
            )
        });
        let second = f.start_turn();
        edit(f, "a.txt", b"one\nTWO\nthree\nfour\nfive\nsix\nseven\nEIGHT\nnine\n");
        live_until(f, editor, at, "the live drawing", |_, e| e.count(LIVE_SHOW_LUA) == 1);
        for _ in 0..10 {
            step(f, editor.rpc(), at);
        }
        (first, second)
    }

    #[test]
    fn panel_shows_leaves_a_live_drawing_of_another_turn() {
        let mut f = live("live-panel-leaves");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let (first, _) = drawn_while_running(&mut f, &editor, at);
        let jobs = f.flow.jobs_started();
        f.flow.panel_shows(&f.tabs, f.tab, first, Scope::Turn, "a.txt");
        assert_eq!(
            f.flow.jobs_started(),
            jobs,
            "nothing is read for a file drawn live for another turn"
        );
        for _ in 0..20 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(editor.count(SHOW_LUA), 0, "the live drawing stays");
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn panel_shows_replaces_it_for_the_same_turn() {
        let mut f = live("live-panel-replaces");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let n = settled_once(&mut f, &editor, at);
        for _ in 0..10 {
            step(&mut f, editor.rpc(), at);
        }
        // The same turn, seen in the session's scope: the panel's drawing replaces the live one.
        let jobs = f.flow.jobs_started();
        f.flow.panel_shows(&f.tabs, f.tab, n, Scope::Session, "a.txt");
        assert_eq!(f.flow.jobs_started(), jobs + 1, "the panel's own turn is redrawn");
        live_until(&mut f, &editor, at, "the panel's drawing", |_, e| {
            e.count(SHOW_LUA) == 1
        });
        let args = editor.args(SHOW_LUA, 0);
        assert_eq!(meta_field(&args, "turn"), Value::from(n));
        assert_eq!(meta_field(&args, "scope"), Value::from("session"));
        f.shut_down();
    }

    #[test]
    fn panel_shows_of_the_running_turn_leaves_its_live_drawing() {
        let mut f = live("live-panel-running");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let (_, second) = drawn_while_running(&mut f, &editor, at);
        let before = editor.asked.borrow().len();
        // The running turn has no end snapshot, so the panel has no hunks to draw in its place.
        let jobs = f.flow.jobs_started();
        f.flow.panel_shows(&f.tabs, f.tab, second, Scope::Turn, "a.txt");
        assert_eq!(f.flow.jobs_started(), jobs + 1, "the panel's own turn is read");
        for _ in 0..40 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        f.settle_file_jobs(editor.rpc());
        let after: Vec<&str> = editor.asked.borrow()[before..].iter().map(|(code, _)| *code).collect();
        assert!(
            !after
                .iter()
                .any(|code| [CLEAR_LUA, LIVE_CLEAR_LUA, SHOW_LUA].contains(code)),
            "the live drawing is neither cleared nor replaced"
        );
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn panel_shows_of_another_turn_drops_an_older_redraw_of_the_file() {
        let mut f = live("live-panel-drops");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let earlier = f.turn(|f| f.write("a.txt", b"ONE\ntwo\nthree\nfour\n", 0o644));
        let n = settled_once(&mut f, &editor, at);
        for _ in 0..10 {
            step(&mut f, editor.rpc(), at);
        }
        let jobs = f.flow.jobs_started();
        // The live drawing's own turn, in the session's scope: read to replace it.
        f.flow.panel_shows(&f.tabs, f.tab, n, Scope::Session, "a.txt");
        assert_eq!(f.flow.jobs_started(), jobs + 1);
        // Then another turn, before that read is in: the live drawing stays, and so does nothing else.
        f.flow.panel_shows(&f.tabs, f.tab, earlier, Scope::Turn, "a.txt");
        for _ in 0..40 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        f.settle_file_jobs(editor.rpc());
        assert_eq!(editor.count(SHOW_LUA), 0, "the read the panel left never draws");
        f.shut_down();
    }

    #[test]
    fn an_o_drawing_on_its_way_keeps_its_headers_from_an_earlier_live_answer() {
        let mut f = live("live-o-on-its-way");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        let (first, second) = drawn_while_running(&mut f, &editor, at);
        // The settle's drawing goes out, and its answer waits.
        editor.hold.borrow_mut().push(LIVE_SHOW_LUA);
        f.end_turn();
        live_until(&mut f, &editor, at, "the settle's drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 2
        });
        assert!(!live_shows(&editor)[1].2, "the settle");
        let panel = panel_hunks(&f, SESSION, first, "a.txt").remove(0);
        let other = panel_hunks(&f, SESSION, second, "a.txt").remove(0);
        assert_ne!(other.header, panel.header, "the two turns' hunks differ");

        // `o` on the first turn queues behind it.
        let abs = f.file("a.txt");
        let mut outs = f.handle(editor.rpc(), open_in_editor("o1", f.tab, first, "a.txt"));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f.flow.local_overlay().unwrap().draws_pending(&abs) {
            assert!(Instant::now() < deadline, "the open never queued");
            outs.extend(step(&mut f, editor.rpc(), at));
            std::thread::sleep(Duration::from_millis(5));
        }
        // The settle's answer comes in; the open goes out and its own answer waits.
        editor.hold.borrow_mut().clear();
        editor.hold.borrow_mut().push(OPEN_AND_SHOW_LUA);
        editor.release();
        for _ in 0..10 {
            outs.extend(step(&mut f, editor.rpc(), at));
        }
        assert_eq!(editor.count(OPEN_AND_SHOW_LUA), 1, "the open is out");
        editor.hold.borrow_mut().clear();
        editor.release();
        while !answered(&outs, "o1") {
            assert!(Instant::now() < deadline, "the open was never answered");
            outs.extend(step(&mut f, editor.rpc(), at));
            std::thread::sleep(Duration::from_millis(5));
        }

        editor.queue_event(revert_event(&f, first, "a.txt", panel.id, 2, "two", "TWO", "lf"));
        tick_until(&mut f, &editor, "the revert", |outs| {
            shape(outs).iter().any(|w| w.starts_with("review_draft"))
        });
        let records = f.draft().reverts();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].new.hunk.as_ref().unwrap().1,
            panel.header,
            "recorded under the header of the turn `o` drew"
        );
        f.shut_down();
    }

    #[test]
    fn a_new_editor_keeps_the_contested_paths() {
        let mut f = live("live-contest-kept");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let (other, provider) = second_tab(&mut f, "other-session");
        let (me, mine) = (f.tab, f.provider.clone());
        begin(&mut f, &mine, SESSION, "a1");
        begin(&mut f, &provider, "other-session", "b1");
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nthree\nfour\n");
        live_until(&mut f, &editor, at, "the first drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        edit_by(&mut f, other, &provider, "b1", "a.txt", b"one\nTWO\nTHREE\nfour\n");
        live_until(&mut f, &editor, at, "the contest's clear", |_, e| {
            e.count(LIVE_CLEAR_LUA) >= 1
        });
        let abs = f.file("a.txt");
        assert!(f.flow.local_overlay().unwrap().is_contested(&abs));

        // The companion detaches and attaches again: another editor, the same turns' edits.
        f.flow.set_editor_owner(None);
        f.flow.set_editor_owner(Some(Owner::Companion { channel: 9 }));
        assert!(
            f.flow.local_overlay().unwrap().is_contested(&abs),
            "the new editor's driver knows the file is contested"
        );
        let before = editor.count(LIVE_SHOW_LUA);
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nTHREE\nFOUR\n");
        finish(&mut f, &mine, SESSION, "a1");
        finish(&mut f, &provider, "other-session", "b1");
        live_settled(&mut f, &editor, at);
        assert_eq!(
            editor.count(LIVE_SHOW_LUA),
            before,
            "only one turn edited it since the new editor came, and it is still not drawn"
        );
        f.shut_down();
    }

    #[test]
    fn a_following_tabs_file_is_drawn_once_the_follow_opened_it() {
        let mut f = live("live-follow");
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        editor.hidden.borrow_mut().push(f.file("a.txt"));
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the shown question", |_, e| {
            e.count(LIVE_SHOWN_LUA) == 1
        });
        for _ in 0..20 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(editor.count(LIVE_SHOW_LUA), 0, "not shown yet");

        // Following opened the file in a split: its end asks again at once, not a second later.
        editor.hidden.borrow_mut().clear();
        f.flow.follow_ends().ended(f.tab.0, PathBuf::from("a.txt"));
        live_until(&mut f, &editor, at, "the drawing", |_, e| e.count(LIVE_SHOW_LUA) == 1);
        assert_eq!(editor.count(LIVE_SHOWN_LUA), 2);
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn closing_a_tab_clears_its_live_drawings() {
        let mut f = live("live-closed");
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the live drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        let spare = f.tabs.open(eitri_core::tab_set::AgentId::Claude);
        let mut closed = f.tabs.remove(f.tab).expect("the tab was open");
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        f.tab = spare;
        live_until(&mut f, &editor, at, "the clear", |_, e| e.count(LIVE_CLEAR_LUA) == 1);
        assert_eq!(live_clears(&editor), vec![path_bytes(&f.file("a.txt"))]);
        f.shut_down();
    }

    #[test]
    fn a_moved_chats_live_drawings_are_cleared_at_the_move() {
        let mut f = live("live-moved");
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        live_until(&mut f, &editor, at, "the live drawing", |_, e| {
            e.count(LIVE_SHOW_LUA) == 1
        });
        let (inside, release) = f.provider.hold_next_pump();
        inside.recv_timeout(Duration::from_secs(10)).expect("ingestion is held");
        let mut release = Some(release);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_folding = false;
        loop {
            match f.tabs.freeze_for_move(f.tab) {
                Err(eitri_core::tab_set::MoveRefusedWhy::Folding) if Instant::now() < deadline => {
                    saw_folding = true;
                    drop(release.take());
                    std::thread::sleep(Duration::from_millis(1));
                }
                other => {
                    other.expect("the chat can move");
                    break;
                }
            }
        }
        assert!(saw_folding, "the held ingestion batch refused the first freeze");
        live_until(&mut f, &editor, at, "the clear", |_, e| e.count(LIVE_CLEAR_LUA) == 1);
        assert_eq!(live_clears(&editor), vec![path_bytes(&f.file("a.txt"))]);
        f.tabs.unfreeze(f.tab);
        f.shut_down();
    }

    #[test]
    fn review_live_false_does_nothing() {
        let mut f = Fixture::new("live-off");
        f.tabs.set_live_edits(false);
        embedded(&mut f);
        f.write("a.txt", FOUR, 0o644);
        f.start_turn();
        let editor = ScriptedEditor::new();
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        for _ in 0..20 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        f.end_turn();
        for _ in 0..20 {
            step(&mut f, editor.rpc(), at);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(editor.count(LIVE_SHOWN_LUA), 0, "no shown question");
        assert_eq!(editor.count(REVIEW_LUA), 0, "no module installed");
        assert!(editor.asked.borrow().is_empty(), "the editor is never asked anything");
        assert_eq!(f.flow.jobs_started(), 0, "no job");
        assert!(f.flow.live_is_idle());
        f.shut_down();
    }

    /// A note as window 1 got it: the turn, its end once it ended, the paths.
    type Note = (TurnKey, Option<u64>, Vec<PathBuf>);

    /// Window 2's way to window 1: every note recorded, and a running turn's note answered with
    /// every path shown.
    #[derive(Default)]
    struct Asked {
        notes: RefCell<Vec<Note>>,
        answers: RefCell<BTreeMap<u64, Vec<bool>>>,
        next: Cell<u64>,
    }

    struct SharedAsk(Rc<Asked>);

    impl LiveAsk for SharedAsk {
        fn note(&self, key: &TurnKey, _started_ns: u64, ended_ns: Option<u64>, abs_paths: &[PathBuf]) -> Option<u64> {
            self.0
                .notes
                .borrow_mut()
                .push((key.clone(), ended_ns, abs_paths.to_vec()));
            if ended_ns.is_some() {
                return None;
            }
            let id = self.0.next.get() + 1;
            self.0.next.set(id);
            self.0.answers.borrow_mut().insert(id, vec![true; abs_paths.len()]);
            Some(id)
        }

        fn take_shown(&self, op_id: u64) -> Option<Result<Vec<bool>, String>> {
            self.0.answers.borrow_mut().remove(&op_id).map(Ok)
        }
    }

    #[test]
    fn live_window_2_notes_and_asks_through_window_1() {
        let mut f = Fixture::new("live-window-2");
        f.tabs.set_live_edits(true);
        f.write("a.txt", FOUR, 0o644);
        let _sent = as_window_2(&mut f, None);
        let asked = Rc::new(Asked::default());
        f.flow.set_live_ask(Box::new(SharedAsk(asked.clone())));
        f.start_turn();
        edit(&mut f, "a.txt", FOUR_EDITED);
        let at = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(20);
        while f.flow.jobs_started() == 0 {
            assert!(Instant::now() < deadline, "the live job never ran");
            step(&mut f, None, at);
            std::thread::sleep(Duration::from_millis(5));
        }
        let key = TurnKey {
            session: SESSION.to_owned(),
            turn_id: "t1".to_owned(),
        };
        assert!(
            asked
                .notes
                .borrow()
                .iter()
                .any(|(k, end, paths)| *k == key && end.is_none() && *paths == [f.file("a.txt")]),
            "the edit is noted through window 1: {:?}",
            asked.notes.borrow()
        );
        assert!(asked.answers.borrow().is_empty(), "every answer was taken");
        f.end_turn();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !asked
            .notes
            .borrow()
            .iter()
            .any(|(k, end, _)| *k == key && end.is_some())
        {
            assert!(Instant::now() < deadline, "the turn's end was never noted");
            step(&mut f, None, at);
            std::thread::sleep(Duration::from_millis(5));
        }
        f.shut_down();
    }

    // ---- linked windows --------------------------------------------------------------------------------

    use eitri_core::nvim_rpc::RpcError;
    use eitri_core::review_editor::{
        EditorOverlay, LinkedEvent, LiveMeta, OverlayCall, OverlayOutcome, RemoteLiveAsk, ShowHunk, Window,
    };
    use eitri_core::window_link::editor_ops::{dispatch, Dispatch, OpContext};
    use eitri_core::wm::PeerId;

    /// The linked window, as window 1 numbers it.
    const PEER: PeerId = PeerId(2);
    /// The session of a tab of window 1's own.
    const HUB_SESSION: &str = "hub-session";

    /// An operation of the linked window, with where its reply goes when one is waited for.
    type Sent = (u64, EditorOp, Option<Answer>);

    /// Window 1 as the linked window reaches it: its review driver over a scripted editor, each of
    /// the linked window's operations dispatched as window 1's link does it, and what the driver
    /// finds for that window handed to its overlay. Its clock runs a second ahead at each round, so
    /// the module's events are drained.
    struct Hub {
        root: PathBuf,
        editor: ScriptedEditor,
        overlay: EditorOverlay,
        sent: Rc<RefCell<Vec<Sent>>>,
        /// The name of every operation dispatched, in order.
        ops: Vec<&'static str>,
        /// Window 1's own outcomes, in order.
        mine: Vec<OverlayOutcome>,
        clock: Instant,
    }

    struct HubCtx<'a> {
        root: &'a Path,
        drawn: Vec<PathBuf>,
    }

    impl OpContext for HubCtx<'_> {
        fn project_root(&self) -> &Path {
            self.root
        }
        fn peer(&self) -> PeerId {
            PEER
        }
        fn session_in_window2(&self, session: &str) -> bool {
            session != HUB_SESSION
        }
        fn plugin_roots_in_window2(&self, _plugin: &str) -> Option<BTreeMap<String, PathBuf>> {
            None
        }
        fn drawn_for_window2(&self) -> Vec<PathBuf> {
            self.drawn.clone()
        }
    }

    /// Makes the fixture's flow a linked window's, reaching `Hub`'s editor; review.live is on.
    fn linked(f: &mut Fixture) -> Hub {
        f.tabs.set_live_edits(true);
        let sent: Rc<RefCell<Vec<Sent>>> = Rc::default();
        let next = Rc::new(Cell::new(0u64));
        let (ops, ids) = (sent.clone(), next.clone());
        let overlay = RemoteOverlay::new(
            &f.project,
            Box::new(move |op| {
                let id = ids.get() + 1;
                ids.set(id);
                ops.borrow_mut().push((id, op, None));
                id
            }),
        );
        let ask = RemoteAsk::new(&f.project, Box::new(|_| None));
        let (ops, ids) = (sent.clone(), next);
        let live_ask = RemoteLiveAsk::new(
            &f.project,
            Box::new(move |op| {
                let id = ids.get() + 1;
                ids.set(id);
                let (answer, pending) = Pending::pair();
                ops.borrow_mut().push((id, op, Some(answer)));
                Some(pending)
            }),
        );
        f.flow.link_to_window_1(overlay, ask);
        f.flow.set_live_ask(Box::new(live_ask));
        Hub {
            root: f.project.clone(),
            editor: ScriptedEditor::new(),
            overlay: EditorOverlay::new(Owner::Embedded),
            sent,
            ops: Vec::new(),
            mine: Vec::new(),
            clock: Instant::now(),
        }
    }

    impl Hub {
        /// One round of window 1: the linked window's operations, a tick of the driver, and its
        /// reports handed to the linked window's overlay.
        fn serve(&mut self, f: &mut Fixture) {
            self.clock += Duration::from_secs(1);
            let at = self.clock;
            let sent: Vec<Sent> = std::mem::take(&mut *self.sent.borrow_mut());
            for (id, op, answer) in sent {
                self.ops.push(op.name());
                let ctx = HubCtx {
                    root: &self.root,
                    drawn: self.overlay.linked_paths(PEER).iter().cloned().collect(),
                };
                match dispatch(id, op, &ctx, &self.editor) {
                    Ok(Dispatch::Overlay(calls)) => {
                        for call in calls {
                            self.overlay
                                .call_for_op(&self.editor, call, Window::Linked(PEER), Some(id), at);
                        }
                    }
                    Ok(Dispatch::LiveNote { note, shown }) => {
                        self.overlay.note_live(
                            Window::Linked(PEER),
                            &note.key,
                            note.started_ns,
                            note.ended_ns,
                            &note.paths,
                        );
                        if let Some(answer) = answer {
                            answer.send(match shown {
                                Some(shown) => shown.wait(Duration::from_secs(60)).expect("the editor answers"),
                                None => Ok(Value::Nil),
                            });
                        }
                    }
                    Ok(Dispatch::Rpc(_)) | Ok(Dispatch::Check(_)) => {}
                    Err(refused) => {
                        if let Some(answer) = answer {
                            answer.send(Err(RpcError::Nvim(refused.to_string())));
                        }
                    }
                }
            }
            let routed = self.overlay.tick(&self.editor, at);
            self.mine.extend(routed.mine);
            let remote = f.flow.remote_overlay().expect("the fixture is a linked window");
            for (peer, events) in routed.linked {
                assert_eq!(peer, PEER);
                for event in events {
                    match event {
                        LinkedEvent::Answered { op_id, result } => remote.answered(op_id, result),
                        LinkedEvent::Drawn { op_id, path, state } => {
                            let rel = path.strip_prefix(&self.root).unwrap().to_str().unwrap();
                            remote.review_drawn_by(op_id, rel, state);
                        }
                        LinkedEvent::Events { session, events } => remote.review_events(&session, events),
                        LinkedEvent::Idle => remote.review_idle(),
                    }
                }
            }
        }

        /// The linked window's live drawings, as its overlay knows them.
        fn remote_drawings(f: &mut Fixture) -> BTreeMap<PathBuf, (String, u32)> {
            f.flow.remote_overlay().unwrap().live_drawings().clone()
        }
    }

    /// Steps the linked window (no editor of its own, its clock at `at`) and window 1 until `done`.
    fn both_until(f: &mut Fixture, hub: &mut Hub, at: Instant, what: &str, done: impl Fn(&mut Fixture, &Hub) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            step(f, None, at);
            hub.serve(f);
            if done(f, hub) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn both_for(f: &mut Fixture, hub: &mut Hub, at: Instant, rounds: usize) {
        for _ in 0..rounds {
            step(f, None, at);
            hub.serve(f);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The linked window's running turn's edit of `a.txt`, drawn in window 1's editor; its number.
    fn linked_drawn_live(f: &mut Fixture, hub: &mut Hub, at: Instant) -> u32 {
        let n = f.start_turn();
        edit(f, "a.txt", FOUR_EDITED);
        let abs = f.file("a.txt");
        both_until(f, hub, at, "the linked window's live drawing", |f, _| {
            Hub::remote_drawings(f).contains_key(&abs)
        });
        n
    }

    #[test]
    fn linked_live_window_2s_edit_is_drawn_in_window_1s_editor() {
        let mut f = Fixture::new("linked-live-drawn");
        f.write("a.txt", FOUR, 0o644);
        let mut hub = linked(&mut f);
        let at = Instant::now();
        let n = linked_drawn_live(&mut f, &mut hub, at);
        let abs = f.file("a.txt");

        // Asked by the shown question and by the edit's note, each one operation that also notes the turn.
        assert!(hub.editor.count(LIVE_SHOWN_LUA) >= 1, "window 1's editor was asked");
        for n in 0..hub.editor.count(LIVE_SHOWN_LUA) {
            assert_eq!(
                hub.editor.args(LIVE_SHOWN_LUA, n)[0],
                Value::Array(vec![Value::Binary(path_bytes(&abs))]),
                "by window 1's own absolute path"
            );
        }
        assert_eq!(live_shows(&hub.editor), vec![(path_bytes(&abs), "t1".to_owned(), true)]);
        let args = hub.editor.args(LIVE_SHOW_LUA, 0);
        assert_eq!(meta_field(&args, "session"), Value::from(SESSION));
        assert_eq!(meta_field(&args, "turn"), Value::from(n));
        let note = hub.ops.iter().position(|op| *op == "review_live_note");
        let show = hub.ops.iter().position(|op| *op == "review_live_show");
        assert!(note.is_some() && note < show, "noted, then drawn: {:?}", hub.ops);

        // Window 1's driver drew it for the linked window, which holds the session, and both know it is live.
        assert!(hub.overlay.linked_paths(PEER).contains(&abs));
        assert_eq!(hub.overlay.holder(SESSION), Window::Linked(PEER));
        let live = BTreeMap::from([(abs.clone(), (SESSION.to_owned(), n))]);
        assert_eq!(hub.overlay.live_drawings(), &live);
        assert_eq!(Hub::remote_drawings(&mut f), live);
        assert!(
            !hub.mine.iter().any(|o| matches!(o, OverlayOutcome::LiveDrawn { .. })),
            "the linked window's drawing is its own news, not window 1's"
        );
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn linked_live_two_windows_editing_one_file_contest_it() {
        let mut f = Fixture::new("linked-live-contest");
        f.write("a.txt", FOUR, 0o644);
        let mut hub = linked(&mut f);
        let abs = f.file("a.txt");
        // Window 1's own turn, running since before the linked window's, has its edit of the file drawn live.
        let mine = TurnKey {
            session: HUB_SESSION.to_owned(),
            turn_id: "h1".to_owned(),
        };
        hub.overlay
            .note_live(Window::One, &mine, 1, None, std::slice::from_ref(&abs));
        let meta = LiveMeta {
            tab: 1,
            session: HUB_SESSION.to_owned(),
            turn_id: "h1".to_owned(),
            turn: 1,
            running: true,
            read_lines: 4,
        };
        let hunk = ShowHunk::from_file_lines(1, (2, 1), (2, 1), &[b"two\n".to_vec()], &[b"TWO\n".to_vec()]);
        hub.overlay.call(
            &hub.editor,
            OverlayCall::LiveShow {
                path: abs.clone(),
                meta,
                hunks: vec![hunk],
            },
            hub.clock,
        );
        let at = Instant::now();
        both_until(&mut f, &mut hub, at, "window 1's own live drawing", |_, hub| {
            hub.overlay.live_drawings().contains_key(&abs)
        });

        // The linked window's turn edits the same file while window 1's runs.
        f.start_turn();
        edit(&mut f, "a.txt", FOUR_EDITED);
        both_until(&mut f, &mut hub, at, "the linked window's live draw", |_, hub| {
            hub.ops.contains(&"review_live_show")
        });
        both_for(&mut f, &mut hub, at, 10);
        assert!(hub.overlay.is_contested(&abs));
        assert_eq!(
            live_shows(&hub.editor).len(),
            1,
            "only window 1's own drawing went up; the linked window's draw of a contested file is not sent"
        );
        assert_eq!(
            live_clears(&hub.editor),
            vec![path_bytes(&abs)],
            "window 1's drawing is taken off"
        );
        assert!(hub.overlay.live_drawings().is_empty());
        assert!(Hub::remote_drawings(&mut f).is_empty());
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn linked_live_gone_clears_window_2s_drawings() {
        let mut f = Fixture::new("linked-live-gone");
        f.write("a.txt", FOUR, 0o644);
        let mut hub = linked(&mut f);
        let at = Instant::now();
        linked_drawn_live(&mut f, &mut hub, at);
        let abs = f.file("a.txt");
        let clears = hub.editor.count(CLEAR_LUA);
        hub.overlay.linked_gone(PEER);
        for _ in 0..10 {
            hub.clock += Duration::from_secs(1);
            hub.overlay.tick(&hub.editor, hub.clock);
        }
        assert_eq!(hub.editor.count(CLEAR_LUA), clears + 1);
        assert_eq!(
            hub.editor.args(CLEAR_LUA, clears)[0],
            Value::Binary(path_bytes(&abs)),
            "the gone window's live drawing is taken off"
        );
        assert!(hub.overlay.live_drawings().is_empty());
        assert!(hub.overlay.linked_paths(PEER).is_empty());
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn linked_live_reverts_go_to_the_window_holding_the_session() {
        let mut f = Fixture::new("linked-live-reverts");
        f.write("a.txt", FOUR, 0o644);
        let mut hub = linked(&mut f);
        let at = Instant::now();
        let n = linked_drawn_live(&mut f, &mut hub, at);
        f.end_turn();
        both_until(&mut f, &mut hub, at, "the settle's drawing", |_, hub| {
            hub.editor.count(LIVE_SHOW_LUA) == 2
        });
        // Let the settle's report reach the linked window's flow.
        both_for(&mut f, &mut hub, at, 10);
        let abs = f.file("a.txt");
        assert_eq!(
            meta_field(&hub.editor.args(LIVE_SHOW_LUA, 1), "running"),
            Value::from(false)
        );
        assert_eq!(Hub::remote_drawings(&mut f)[&abs], (SESSION.to_owned(), n));

        let panel = panel_hunks(&f, SESSION, n, "a.txt").remove(0);
        assert_ne!(
            panel.header, "@@ -2 +2 @@",
            "the panel's header is not the one counts would give"
        );
        hub.editor
            .queue_event(revert_event(&f, n, "a.txt", panel.id, 2, "two", "TWO", "lf"));
        both_until(
            &mut f,
            &mut hub,
            at,
            "the revert in the linked window's draft",
            |f, _| !f.draft().reverts().is_empty(),
        );
        let records = f.draft().reverts();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].new.hunk.as_ref().unwrap().1,
            panel.header,
            "recorded under the panel's header, which the settle's drawing carried"
        );
        assert!(
            !hub.mine
                .iter()
                .any(|o| matches!(o, OverlayOutcome::Reverts(_) | OverlayOutcome::Unreverts(_))),
            "window 1 got none of it"
        );
        f.shut_down();
    }

    #[test]
    fn linked_live_panel_shows_leaves_window_1s_live_drawing() {
        let mut f = Fixture::new("linked-live-panel");
        f.write(
            "a.txt",
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
            0o644,
        );
        let first = f.turn(|f| {
            f.write(
                "a.txt",
                b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
                0o644,
            )
        });
        let mut hub = linked(&mut f);
        let at = Instant::now();
        let second = f.start_turn();
        edit(
            &mut f,
            "a.txt",
            b"one\nTWO\nthree\nfour\nfive\nsix\nseven\nEIGHT\nnine\n",
        );
        let abs = f.file("a.txt");
        both_until(&mut f, &mut hub, at, "the linked window's live drawing", |f, _| {
            Hub::remote_drawings(f).contains_key(&abs)
        });

        // The linked window's panel looks at the first turn of the file drawn live for the second.
        let jobs = f.flow.jobs_started();
        f.flow.panel_shows(&f.tabs, f.tab, first, Scope::Turn, "a.txt");
        assert_eq!(
            f.flow.jobs_started(),
            jobs,
            "nothing is read for a file drawn live for another turn"
        );
        both_for(&mut f, &mut hub, at, 20);
        assert!(!hub.ops.contains(&"review_show"), "{:?}", hub.ops);
        assert_eq!(
            hub.editor.count(SHOW_LUA),
            0,
            "the live drawing in window 1's editor stays"
        );
        assert_eq!(hub.overlay.live_drawings()[&abs], (SESSION.to_owned(), second));
        f.end_turn();
        f.shut_down();
    }

    #[test]
    fn linked_live_a_turn_that_moved_to_window_1_keeps_its_end_when_the_link_ends() {
        let mut f = live("linked-live-moved");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let abs = f.file("a.txt");
        let (me, mine) = (f.tab, f.provider.clone());
        // The linked window noted a1 as running; then its session came to window 1, which runs the turn
        // to its end.
        let moved = TurnKey {
            session: SESSION.to_owned(),
            turn_id: "a1".to_owned(),
        };
        let started_ns = eitri_core::turn_review::monotonic_ns();
        assert!(f
            .flow
            .note_linked_live(PEER, &moved, started_ns, None, std::slice::from_ref(&abs)));
        begin(&mut f, &mine, SESSION, "a1");
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nthree\nfour\n");
        live_until(&mut f, &editor, at, "a1's drawing", |_, e| e.count(LIVE_SHOW_LUA) == 1);
        finish(&mut f, &mine, SESSION, "a1");
        live_settled(&mut f, &editor, at);

        // Another tab's turn edits the file after a1 ended.
        let (other, provider) = second_tab(&mut f, "other-session");
        begin(&mut f, &provider, "other-session", "b1");
        edit_by(&mut f, other, &provider, "b1", "a.txt", b"one\nTWO\nTHREE\nfour\n");
        live_until(&mut f, &editor, at, "b1's drawing", |_, e| e.count(LIVE_SHOW_LUA) == 2);
        assert!(!f.flow.local_overlay().unwrap().is_contested(&abs));

        // The link ends: a1 is window 1's now, and its end stays the one window 1 noted.
        f.flow.linked_live_gone(PEER, eitri_core::turn_review::monotonic_ns());
        assert!(
            !f.flow.local_overlay().unwrap().is_contested(&abs),
            "a1 ended before b1 began, whatever the link's end says"
        );
        finish(&mut f, &provider, "other-session", "b1");
        f.shut_down();
    }

    #[test]
    fn linked_live_a_note_while_window_1_has_no_editor_reaches_the_next_driver() {
        let mut f = live("linked-live-parked");
        let editor = ScriptedEditor::new();
        let at = Instant::now();
        f.write("a.txt", FOUR, 0o644);
        let abs = f.file("a.txt");
        let linked = TurnKey {
            session: "linked-session".to_owned(),
            turn_id: "l1".to_owned(),
        };
        let started_ns = eitri_core::turn_review::monotonic_ns();
        assert!(f
            .flow
            .note_linked_live(PEER, &linked, started_ns, None, std::slice::from_ref(&abs)));

        // The editor goes; the linked turn ends meanwhile, and a second one starts and the link ends.
        f.flow.set_editor_owner(None);
        assert!(
            f.flow.note_linked_live(
                PEER,
                &linked,
                started_ns,
                Some(eitri_core::turn_review::monotonic_ns()),
                &[]
            ),
            "a window without an editor still takes the note"
        );
        let gone = TurnKey {
            session: "linked-session".to_owned(),
            turn_id: "l2".to_owned(),
        };
        let started_ns = eitri_core::turn_review::monotonic_ns();
        assert!(f
            .flow
            .note_linked_live(PEER, &gone, started_ns, None, std::slice::from_ref(&abs)));
        f.flow.linked_live_gone(PEER, eitri_core::turn_review::monotonic_ns());
        f.flow.set_editor_owner(Some(Owner::Embedded));

        // A later turn of window 1's that edits the file contests neither.
        let (me, mine) = (f.tab, f.provider.clone());
        begin(&mut f, &mine, SESSION, "a1");
        edit_by(&mut f, me, &mine, "a1", "a.txt", b"one\nTWO\nthree\nfour\n");
        live_until(&mut f, &editor, at, "a1's drawing", |_, e| e.count(LIVE_SHOW_LUA) == 1);
        assert!(!f.flow.local_overlay().unwrap().is_contested(&abs));
        finish(&mut f, &mine, SESSION, "a1");
        f.shut_down();
    }
}

#[test]
fn an_unreadable_recovery_entry_warns_and_offers_the_readable_entry_without_retry() {
    recovery_entry_problem(false);
}

#[test]
fn an_unrecognized_recovery_entry_warns_and_offers_the_readable_entry_without_retry() {
    recovery_entry_problem(true);
}

fn recovery_entry_problem(malformed: bool) {
    let mut f = Fixture::new("recover-entry-problem");
    let journal = Journal::open(&f.review_dir).unwrap();
    let good_id = "0123456789abcdef0123456789abcdef";
    let bad_id = "fedcba9876543210fedcba9876543210";
    for id in [good_id, bad_id] {
        let entry = json!({
            "v": 1, "id": id, "path_hex": "612e747874", "pre": null, "intended": null,
            "mode": 0o600, "session": SESSION, "at_ms": 1, "pid": 1
        });
        std::fs::write(journal.dir().join(format!("{id}.json")), entry.to_string()).unwrap();
    }
    let bad = journal.dir().join(format!("{bad_id}.json"));
    if malformed {
        std::fs::write(&bad, b"{ not an entry").unwrap();
    } else {
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let mut outs = f.flow.document_ready(&f.tabs);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.flow.is_idle() {
        assert!(Instant::now() < deadline, "journal scan did not finish");
        outs.extend(f.tick(None));
        std::thread::yield_now();
    }
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(bad.exists(), "the problem entry is kept");
    assert_eq!(offered(&outs), Some(vec![good_id.to_string()]));
    let notices: Vec<_> = envelopes(&outs).into_iter().filter(|v| v["kind"] == "notice").collect();
    assert_eq!(notices.len(), 1, "one warning for the scan's problems: {notices:?}");
    assert!(
        notices[0].to_string().contains(bad_id),
        "the warning names the bad entry"
    );
    assert_eq!(f.flow.jobs_started(), 1);
    let reload = f.flow.document_ready(&f.tabs);
    assert_eq!(offered(&reload), Some(vec![good_id.to_string()]));
    assert!(!envelopes(&reload).iter().any(|v| v["kind"] == "notice"));
    assert_eq!(f.flow.jobs_started(), 1, "entry problems do not trigger another scan");
    f.shut_down();
}

#[test]
fn an_unreadable_recovery_journal_warns_and_the_next_page_load_retries() {
    let mut f = Fixture::new("recover-retry");
    let journal = f.review_dir.join("journal");
    std::fs::create_dir_all(&f.review_dir).unwrap();
    std::fs::write(&journal, b"blocks journal directory").unwrap();
    let mut outs = f.flow.document_ready(&f.tabs);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.flow.is_idle() {
        assert!(Instant::now() < deadline, "journal scan did not finish");
        outs.extend(f.tick(None));
        std::thread::yield_now();
    }
    assert!(
        outs.iter().any(
            |out| matches!(out, Out::Envelope(text) if text.contains("could not check for an interrupted revert:"))
        ),
        "no warning"
    );
    assert_eq!(f.flow.jobs_started(), 1);
    std::fs::remove_file(&journal).unwrap();
    assert!(f.flow.document_ready(&f.tabs).is_empty());
    while !f.flow.is_idle() {
        assert!(Instant::now() < deadline, "retry did not finish");
        f.tick(None);
        std::thread::yield_now();
    }
    assert_eq!(f.flow.jobs_started(), 2, "failed scan retried");
    assert!(f.flow.document_ready(&f.tabs).is_empty());
    assert_eq!(f.flow.jobs_started(), 2, "successful empty scan is remembered");
    f.shut_down();
}
