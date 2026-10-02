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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::{AgentConversation, AgentDomainEvent, TurnOutcome};
use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::{parse_inbound_message, InboundMessage, SessionModeChoice};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::review_editor::{Owner, CLEAR_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, SHOW_LUA, TAKE_EVENTS_LUA};
use eitri_core::tab_set::{TabBackend, TabSet};
use eitri_core::tabs::TabId;
use eitri_core::test_providers::RecordingProvider;
use eitri_core::turn_review::{
    check_reverts, replace, review_dir_for, saved_to_undo, show_hunk, undo_to_saved, Blocked, Busy, Content, FsHooks,
    Journal, JournalNote, NewRevert, Out, PresenceGuard, PresenceHolder, ProjectDir, RevertShape, RevertSource,
    RevertStatus, ReviewFlow, ReviewOptions, Saved, Scope, Shadow, Snap, Stage, TurnReview, UndoState,
};
use rmpv::Value;
use serde_json::{json, Value as Json};

const SESSION: &str = "flow-session";
const CHILD_DIR: &str = "EITRI_FLOW_CHILD_DIR";
const CHILD_REL: &str = "EITRI_FLOW_CHILD_REL";
/// A pid no real window here has.
const OTHER_PID: u32 = 999_999;

// ---- scratch and files -----------------------------------------------------------------------------

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_flow")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

/// The envelopes among `outs`, parsed.
fn envelopes(outs: &[Out]) -> Vec<Json> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Envelope(text) => Some(serde_json::from_str(text).expect("an envelope is JSON")),
            Out::Flushed(..) | Out::QueueChanged(..) | Out::EditorOpened => None,
        })
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
            Out::Envelope(text) => {
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
    let conversation = AgentConversation::create(provider.clone(), project).unwrap();
    (provider, AgentBackend::Sidecar(Box::new(conversation)))
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut f = Fixture::bare(name);
        f.give_session();
        f
    }

    /// A tab set with a review and a held presence, and one empty tab.
    fn bare(name: &str) -> Fixture {
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
        tabs.hold_presence(PresenceHolder::start(review_dir.clone(), std::process::id()));
        let guard = tabs.presence_guard().expect("presence is held");
        until("this window's presence", || guard.check().is_ok());
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
        self.tabs.get_mut(self.tab).unwrap().backend = TabBackend::Live(backend);
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
        for mut tab in self.tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
    let spare = f.tabs.open();
    let editor = FakeEditor::holding();
    let outs = f.handle(editor.rpc(), revert_file("r1", f.tab, n, "a.txt"));
    assert!(outs.is_empty());
    let before = fingerprint(&f.file("a.txt"));

    let mut closed = f.tabs.remove(f.tab).expect("the tab was open");
    if let TabBackend::Live(backend) = &mut closed.backend {
        backend.shutdown();
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
    let old = std::mem::replace(&mut f.tabs.get_mut(f.tab).unwrap().backend, TabBackend::Live(backend));
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
    tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
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
    for mut tab in tabs.take_all() {
        if let TabBackend::Live(backend) = &mut tab.backend {
            backend.shutdown();
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
    let _spare = f.tabs.open();
    let mut outs = f.handle(None, comment("c1", f.tab, n, "a.txt", "hello"));
    let mut closed = f.tabs.remove(f.tab).unwrap();
    if let TabBackend::Live(backend) = &mut closed.backend {
        backend.shutdown();
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

/// The child of the recovery tests: an in-place rewrite of a hard-linked file that dies after its
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
    let _ = replace(&target, &content, &note, &journal, &shadow, &hooks);
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

/// Kills an in-place write of `rel` halfway (the file gets a second link outside the project, so
/// it is rewritten in place) and returns the id of the journal entry it left.
fn crash_mid_write(f: &Fixture, rel: &str) -> String {
    let link = f.scratch.0.join("second-link");
    std::fs::hard_link(f.file(rel), link).unwrap();
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
        found = journal.pending().into_iter().find(|e| e.path == Path::new(rel));
        found.is_some()
    });
    found.unwrap().id
}

fn pending_ids(f: &Fixture) -> Vec<String> {
    Journal::open(&f.review_dir)
        .unwrap()
        .pending()
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
        self.asked.borrow_mut().push((code, args));
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
        } else {
            Value::Nil
        };
        let (reply, pending) = Pending::pair();
        reply.send(Ok(answer));
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
    let spare = f.tabs.open();
    let editor = ScriptedEditor::new();
    f.run(editor.rpc(), "o1", open_in_editor("o1", f.tab, n, "a.txt"));
    editor.queue_event(revert_event(&f, n, "a.txt", 1, 2, "two", "TWO", "lf"));
    let gone = f.tab;
    let mut closed = f.tabs.remove(gone).expect("the tab was open");
    if let TabBackend::Live(backend) = &mut closed.backend {
        backend.shutdown();
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
    let files = [
        root.join("src/review_editor.rs"),
        root.join("src/turn_review/flow.rs"),
        root.join("../shell/src/review_editor.rs"),
    ];
    // Spelled in pieces so this file never contains what it looks for.
    let needles = [["send", "_keys"].concat(), ["input", "_keys"].concat()];
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for needle in &needles {
            assert!(!text.contains(needle.as_str()), "{} mentions {needle}", file.display());
        }
    }
}
