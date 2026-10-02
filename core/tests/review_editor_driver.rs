//! The review overlay driver against a fake editor that records what it is sent and answers when
//! the test says so: the install before the first call, the one re-install, a new editor, the
//! paced drain of the module's events, a failing editor, and `editor_lost`. One `#[ignore]`d case
//! runs the whole thing through an `NvimLink` to a real `nvim --embed --listen`.

#[path = "support/embed_client.rs"]
mod embed_client;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, NvimLink, Pending, RpcError};
use eitri_core::review_editor::{
    install_args, EditorOverlay, OffEvent, OverlayCall, OverlayOutcome, Owner, ShowHunk, ShowMeta, CLEAR_LUA,
    OPEN_AND_SHOW_LUA, REVIEW_LUA, SHOW_LUA, TAKE_EVENTS_LUA,
};
use eitri_core::turn_review::Scope;
use rmpv::Value;

struct Sent {
    code: &'static str,
    args: Vec<Value>,
    answer: Option<Answer>,
}

/// An editor that records every call and leaves it unanswered until the test answers it.
struct FakeEditor {
    sent: RefCell<Vec<Sent>>,
    target: Cell<Option<u64>>,
}

impl FakeEditor {
    fn new() -> FakeEditor {
        FakeEditor {
            sent: RefCell::new(Vec::new()),
            target: Cell::new(Some(1)),
        }
    }

    /// The code of every call so far, in the order sent.
    fn codes(&self) -> Vec<&'static str> {
        self.sent.borrow().iter().map(|s| s.code).collect()
    }

    fn count(&self, code: &str) -> usize {
        self.codes().iter().filter(|c| **c == code).count()
    }

    /// Answers the oldest unanswered call; panics when the oldest is not `code`, so a test that
    /// expects calls in an order says so.
    fn answer(&self, code: &str, reply: Result<Value, RpcError>) {
        let mut sent = self.sent.borrow_mut();
        let next = sent
            .iter_mut()
            .find(|s| s.answer.is_some())
            .expect("a call waiting for its answer");
        assert_eq!(next.code, code, "the oldest unanswered call");
        next.answer.take().unwrap().send(reply);
    }

    /// Answers the oldest unanswered call whose code is `code`, whatever was sent before it.
    fn answer_oldest_of(&self, code: &str, reply: Result<Value, RpcError>) {
        let mut sent = self.sent.borrow_mut();
        let next = sent
            .iter_mut()
            .find(|s| s.answer.is_some() && s.code == code)
            .expect("a call of that code waiting for its answer");
        next.answer.take().unwrap().send(reply);
    }

    fn unanswered(&self) -> usize {
        self.sent.borrow().iter().filter(|s| s.answer.is_some()).count()
    }
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.sent.borrow_mut().push(Sent {
            code,
            args,
            answer: Some(answer),
        });
        pending
    }

    fn target(&self) -> Option<u64> {
        self.target.get()
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

fn installed() -> Result<Value, RpcError> {
    Ok(map(vec![
        ("installed", Value::from(true)),
        ("replaced", Value::from(false)),
    ]))
}

fn missing() -> Result<Value, RpcError> {
    Ok(map(vec![("missing", Value::from(true))]))
}

fn meta() -> ShowMeta {
    ShowMeta {
        tab: 3,
        session: "s".to_owned(),
        turn: 2,
        scope: Scope::Turn,
    }
}

fn open(request_id: &str, path: &str) -> OverlayCall {
    OverlayCall::OpenAndShow {
        request_id: request_id.to_owned(),
        path: PathBuf::from(path),
        line: Some(4),
        meta: meta(),
        hunks: Some(vec![]),
    }
}

fn show(path: &str) -> OverlayCall {
    OverlayCall::Show {
        path: PathBuf::from(path),
        meta: meta(),
        hunks: vec![],
    }
}

fn clear(path: &str) -> OverlayCall {
    OverlayCall::Clear {
        path: PathBuf::from(path),
    }
}

fn answered_show(drawn: u64, active: u64) -> Result<Value, RpcError> {
    Ok(map(vec![
        ("drawn", Value::from(drawn)),
        ("skipped", Value::from(0)),
        ("notice", Value::Nil),
        ("active", Value::from(active)),
    ]))
}

fn events_answer(events: Vec<Value>, active: u64) -> Result<Value, RpcError> {
    Ok(map(vec![
        ("events", Value::Array(events)),
        ("active", Value::from(active)),
    ]))
}

/// An answer that could not hold every waiting event.
fn full_events_answer(events: Vec<Value>, active: u64) -> Result<Value, RpcError> {
    Ok(map(vec![
        ("events", Value::Array(events)),
        ("active", Value::from(active)),
        ("more", Value::from(true)),
    ]))
}

/// The module's `revert` event for a one-line hunk on `path`.
fn revert_event(path: &str, hunk: u32, old: &str, new: &str) -> Value {
    map(vec![
        ("kind", Value::from("revert")),
        ("tab", Value::from(3)),
        ("session", Value::from("s")),
        ("turn", Value::from(2)),
        ("scope", Value::from("turn")),
        ("path", Value::from(path)),
        ("hunk_id", Value::from(hunk)),
        ("old_start", Value::from(5)),
        ("old_len", Value::from(1)),
        ("new_start", Value::from(5)),
        ("new_len", Value::from(1)),
        ("at_line", Value::from(6)),
        ("old_lines", Value::Array(vec![Value::from(old)])),
        ("new_lines", Value::Array(vec![Value::from(new)])),
        ("old_eols", Value::Array(vec![Value::from("lf")])),
        ("new_eols", Value::Array(vec![Value::from("lf")])),
    ])
}

fn off_event(path: &str, why: &str) -> Value {
    map(vec![
        ("kind", Value::from("off")),
        ("path", Value::from(path)),
        ("why", Value::from(why)),
    ])
}

fn ms(base: Instant, n: u64) -> Instant {
    base + Duration::from_millis(n)
}

#[test]
fn installs_once_before_the_first_call() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Companion { channel: 9 });
    let t0 = Instant::now();

    overlay.call(&rpc, clear("/p/a"), t0);
    assert_eq!(rpc.codes(), vec![REVIEW_LUA], "the install goes first, alone");
    assert_eq!(
        rpc.sent.borrow()[0].args,
        install_args(Owner::Companion { channel: 9 }),
        "it names its owner"
    );
    overlay.call(&rpc, clear("/p/b"), t0);
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA],
        "a second call waits behind the same install"
    );
    assert!(overlay.tick(&rpc, t0).is_empty());
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA],
        "nothing is sent while the install is unanswered"
    );
    assert!(overlay.wants_ticks());

    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA, CLEAR_LUA],
        "the first call goes out once the install answered, alone"
    );
    assert_eq!(
        rpc.sent.borrow()[1].args,
        vec![Value::Binary(b"/p/a".to_vec())],
        "the first call is the first sent"
    );
    overlay.tick(&rpc, t0);
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA, CLEAR_LUA],
        "the second waits for the first's answer"
    );
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.codes(), vec![REVIEW_LUA, CLEAR_LUA, CLEAR_LUA]);
    assert_eq!(rpc.sent.borrow()[2].args, vec![Value::Binary(b"/p/b".to_vec())]);
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);

    overlay.call(&rpc, clear("/p/c"), t0);
    assert_eq!(rpc.count(REVIEW_LUA), 1, "installed once");
    assert_eq!(rpc.codes().last(), Some(&CLEAR_LUA), "and the call goes straight out");
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);
    assert!(!overlay.wants_ticks(), "idle with nothing drawn");
}

#[test]
fn a_missing_module_is_reinstalled_once() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();

    overlay.call(&rpc, open("r1", "/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.codes(), vec![REVIEW_LUA, OPEN_AND_SHOW_LUA]);

    rpc.answer(OPEN_AND_SHOW_LUA, missing());
    assert!(overlay.tick(&rpc, t0).is_empty(), "a first missing is not a failure");
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA],
        "it installs again"
    );
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    assert_eq!(
        rpc.codes(),
        vec![REVIEW_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, OPEN_AND_SHOW_LUA],
        "and sends the call again"
    );

    // Missing a second time: the call fails, and there is no third install.
    rpc.answer(OPEN_AND_SHOW_LUA, missing());
    let out = overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(REVIEW_LUA), 2);
    assert_eq!(out.len(), 1);
    match &out[0] {
        OverlayOutcome::Answered { request_id, result } => {
            assert_eq!(request_id, "r1");
            assert!(result.as_ref().unwrap_err().contains("not in the editor"), "{result:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rpc.unanswered(), 0);
}

#[test]
fn a_reinstall_keeps_the_calls_in_the_order_they_were_made() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, clear("/p/seed"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);

    overlay.call(&rpc, clear("/p/a"), t0);
    overlay.call(&rpc, clear("/p/b"), t0);
    assert_eq!(rpc.count(CLEAR_LUA), 2, "the second call waits behind the first");
    rpc.answer(CLEAR_LUA, missing());
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(REVIEW_LUA), 2, "one re-install for both");
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(REVIEW_LUA), 2, "still one re-install");
    let sent = rpc.sent.borrow();
    let resent: Vec<_> = sent[sent.len() - 2..].iter().map(|s| s.args[0].clone()).collect();
    assert_eq!(
        resent,
        vec![Value::Binary(b"/p/a".to_vec()), Value::Binary(b"/p/b".to_vec())]
    );
}

#[test]
fn a_new_target_forgets_the_install() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();

    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert!(overlay.active_paths().contains(Path::new("/p/a")));
    assert_eq!(rpc.count(REVIEW_LUA), 1);
    let drains = rpc.count(TAKE_EVENTS_LUA);
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 1));

    // nvim restarted behind the same handle.
    rpc.target.set(Some(2));
    overlay.tick(&rpc, ms(t0, 10));
    assert!(
        overlay.active_paths().is_empty(),
        "what was drawn was in the old editor"
    );
    assert_eq!(
        rpc.count(TAKE_EVENTS_LUA),
        drains,
        "and nothing is asked of the new one yet"
    );

    overlay.call(&rpc, show("/p/a"), ms(t0, 20));
    assert_eq!(rpc.count(REVIEW_LUA), 2, "the module is installed again");
    assert_eq!(rpc.codes().last(), Some(&REVIEW_LUA), "before the call");
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, ms(t0, 20));
    assert_eq!(rpc.codes().last(), Some(&SHOW_LUA));
}

/// Two redraws of one file sent together could reach nvim in either order on a transport that puts
/// each request on a thread of its own, and the older one executed last would win.
#[test]
fn a_newer_redraw_is_sent_only_after_the_older_one_answered() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, clear("/p/seed"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, t0);

    let older = OverlayCall::Show {
        path: PathBuf::from("/p/a"),
        meta: ShowMeta { turn: 2, ..meta() },
        hunks: vec![],
    };
    let newer = OverlayCall::Show {
        path: PathBuf::from("/p/a"),
        meta: ShowMeta { turn: 3, ..meta() },
        hunks: vec![],
    };
    overlay.call(&rpc, older, t0);
    overlay.call(&rpc, newer, t0);
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(SHOW_LUA), 1, "the newer redraw waits");
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(SHOW_LUA), 2, "and goes once the older one answered");
    let turns: Vec<Value> = rpc
        .sent
        .borrow()
        .iter()
        .filter(|s| s.code == SHOW_LUA)
        .map(|s| {
            let Value::Map(pairs) = &s.args[1] else {
                panic!("meta is a map")
            };
            pairs
                .iter()
                .find(|(k, _)| k.as_str() == Some("turn"))
                .map(|(_, v)| v.clone())
                .unwrap()
        })
        .collect();
    assert_eq!(
        turns,
        vec![Value::from(2), Value::from(3)],
        "in the order they were made"
    );
}

/// A revert made in the buffer just before the last overlay is cleared waits in the module; the
/// clear's `active: 0` must not leave it there.
#[test]
fn clearing_the_last_overlay_still_takes_the_waiting_events() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1);
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 1));
    overlay.tick(&rpc, ms(t0, 10));

    // Well inside the 500 ms between drains, the user reverts a hunk and the panel clears the file.
    overlay.call(&rpc, clear("/p/a"), ms(t0, 20));
    rpc.answer(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, ms(t0, 30));
    assert_eq!(
        rpc.count(TAKE_EVENTS_LUA),
        2,
        "the last overlay went: the events are asked for once more, at once"
    );
    rpc.answer(
        TAKE_EVENTS_LUA,
        events_answer(vec![revert_event("/p/a", 1, "old", "new")], 0),
    );
    let out = overlay.tick(&rpc, ms(t0, 40));
    assert!(
        matches!(out.as_slice(), [OverlayOutcome::Reverts(events)] if events.len() == 1),
        "{out:?}"
    );
    overlay.tick(&rpc, ms(t0, 5_000));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 2, "and then nothing more");
    assert!(!overlay.wants_ticks());
}

/// The owed drain is sent after the answer that made it owed, even when an earlier drain was
/// still in flight then: that earlier one reached nvim before the clear and proves nothing.
#[test]
fn a_drain_in_flight_during_the_clear_does_not_settle_the_owed_one() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1, "a drain is in flight");

    overlay.call(&rpc, clear("/p/a"), ms(t0, 20));
    rpc.answer_oldest_of(CLEAR_LUA, Ok(map(vec![("active", Value::from(0))])));
    overlay.tick(&rpc, ms(t0, 30));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1, "never two drains in flight");
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 0));
    overlay.tick(&rpc, ms(t0, 40));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 2, "the owed drain follows");
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 0));
    overlay.tick(&rpc, ms(t0, 5_000));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 2);
    assert!(!overlay.wants_ticks());
}

#[test]
fn drains_every_500ms_only_while_active() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();

    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(
        rpc.count(TAKE_EVENTS_LUA),
        1,
        "something is drawn: the first drain is asked at once"
    );
    assert!(rpc.sent.borrow().last().unwrap().args.is_empty());

    // Unanswered, there is never a second one in flight.
    overlay.tick(&rpc, ms(t0, 2_000));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1);
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 1));
    overlay.tick(&rpc, ms(t0, 2_001));
    assert_eq!(
        rpc.count(TAKE_EVENTS_LUA),
        2,
        "the answer came after 500 ms: the next is due"
    );
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![], 1));

    let base = ms(t0, 2_001);
    overlay.tick(&rpc, ms(base, 499));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 2, "not before 500 ms");
    overlay.tick(&rpc, ms(base, 500));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 3, "at 500 ms");

    // The last buffer stops showing: nothing more is asked.
    rpc.answer(TAKE_EVENTS_LUA, events_answer(vec![off_event("/p/a", "user")], 0));
    let out = overlay.tick(&rpc, ms(base, 600));
    assert_eq!(
        out,
        vec![OverlayOutcome::Off(vec![OffEvent {
            path: "/p/a".to_owned(),
            why: "user".to_owned()
        }])]
    );
    assert!(overlay.active_paths().is_empty());
    overlay.tick(&rpc, ms(base, 5_000));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 3, "no drain while nothing is drawn");
    assert!(!overlay.wants_ticks());
}

#[test]
fn events_come_back_in_order() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(2, 1));
    overlay.tick(&rpc, t0);

    rpc.answer(
        TAKE_EVENTS_LUA,
        events_answer(
            vec![
                revert_event("/p/a", 2, "two", "TWO"),
                off_event("/p/b", "closed"),
                revert_event("/p/a", 1, "one", "ONE"),
                map(vec![("kind", Value::from("something-new"))]),
            ],
            1,
        ),
    );
    let out = overlay.tick(&rpc, ms(t0, 1));
    assert_eq!(out.len(), 2, "{out:?}");
    match &out[0] {
        OverlayOutcome::Reverts(reverts) => {
            let ids: Vec<u32> = reverts.iter().map(|r| r.hunk.id).collect();
            assert_eq!(ids, vec![2, 1], "in the order they happened, not by id");
            assert_eq!(reverts[0].path, "/p/a");
            assert_eq!(reverts[0].meta, meta());
            assert_eq!(reverts[0].at_line, 6, "where the lines are now, beside the header");
            assert_eq!(reverts[0].hunk.new_start, 5);
            assert_eq!(reverts[0].hunk.old_bytes(), b"two\n");
            assert_eq!(reverts[1].hunk.new_bytes(), b"ONE\n");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        out[1],
        OverlayOutcome::Off(vec![OffEvent {
            path: "/p/b".to_owned(),
            why: "closed".to_owned()
        }])
    );
    assert!(overlay.tick(&rpc, ms(t0, 2)).is_empty(), "taken once");
}

#[test]
fn a_full_answer_is_followed_at_once_even_with_nothing_drawn() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1);

    // The user reverted and closed the overlay; the module could not fit every event in one answer.
    rpc.answer(
        TAKE_EVENTS_LUA,
        full_events_answer(vec![revert_event("/p/a", 1, "x", "y")], 0),
    );
    let out = overlay.tick(&rpc, ms(t0, 1));
    assert!(
        matches!(out.as_slice(), [OverlayOutcome::Reverts(r)] if r[0].hunk.id == 1),
        "{out:?}"
    );
    assert_eq!(
        rpc.count(TAKE_EVENTS_LUA),
        2,
        "the rest is asked for at once, though nothing is drawn"
    );
    assert!(overlay.wants_ticks());

    rpc.answer(
        TAKE_EVENTS_LUA,
        events_answer(vec![revert_event("/p/a", 2, "x", "y"), off_event("/p/a", "user")], 0),
    );
    let out = overlay.tick(&rpc, ms(t0, 2));
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(matches!(&out[0], OverlayOutcome::Reverts(r) if r[0].hunk.id == 2));
    overlay.tick(&rpc, ms(t0, 5_000));
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 2, "nothing waits and nothing is drawn");
    assert!(!overlay.wants_ticks());
}

#[test]
fn a_path_that_is_not_utf8_is_never_drawn_over() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    let path = PathBuf::from(OsStr::from_bytes(b"/p/\xff.txt"));
    overlay.call(
        &rpc,
        OverlayCall::OpenAndShow {
            request_id: "r1".to_owned(),
            path: path.clone(),
            line: None,
            meta: meta(),
            hunks: Some(vec![]),
        },
        t0,
    );
    overlay.call(
        &rpc,
        OverlayCall::Show {
            path: path.clone(),
            meta: meta(),
            hunks: vec![],
        },
        t0,
    );
    assert!(rpc.codes().is_empty(), "nothing is sent, not even the install");
    let out = overlay.tick(&rpc, t0);
    match out.as_slice() {
        [OverlayOutcome::Answered {
            request_id,
            result: Err(why),
        }] => {
            assert_eq!(request_id, "r1");
            assert!(why.contains("UTF-8"), "{why}");
        }
        other => panic!("{other:?}"),
    }

    // Only opening it draws nothing, so that still goes.
    overlay.call(
        &rpc,
        OverlayCall::OpenAndShow {
            request_id: "r2".to_owned(),
            path,
            line: Some(1),
            meta: meta(),
            hunks: None,
        },
        t0,
    );
    assert_eq!(rpc.codes(), vec![REVIEW_LUA]);
}

#[test]
fn an_event_it_cannot_read_is_dropped_and_the_rest_still_arrive() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    let mut broken = revert_event("/p/a", 7, "x", "y");
    if let Value::Map(entries) = &mut broken {
        entries.retain(|(k, _)| k.as_str() != Some("new_start"));
    }
    rpc.answer(
        TAKE_EVENTS_LUA,
        events_answer(vec![broken, revert_event("/p/a", 8, "x", "y")], 1),
    );
    let out = overlay.tick(&rpc, ms(t0, 1));
    match out.as_slice() {
        [OverlayOutcome::Reverts(reverts)] => assert_eq!(reverts.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unavailable_editor_fails_the_call_with_its_reason() {
    let rpc = FakeEditor::new();
    rpc.target.set(None);
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();

    overlay.call(&rpc, open("r1", "/p/a"), t0);
    overlay.call(&rpc, clear("/p/b"), t0);
    rpc.answer(
        REVIEW_LUA,
        Err(RpcError::Unavailable(
            "the panel is not attached to an editor".to_owned(),
        )),
    );
    let out = overlay.tick(&rpc, t0);
    assert_eq!(
        out,
        vec![OverlayOutcome::Answered {
            request_id: "r1".to_owned(),
            result: Err("no editor to ask: the panel is not attached to an editor".to_owned()),
        }],
        "the call that asked is told why; a clear has nobody to tell"
    );
    assert_eq!(rpc.codes(), vec![REVIEW_LUA], "neither call was sent");
    assert!(!overlay.wants_ticks());

    // Later the editor is there: the next call installs afresh.
    rpc.target.set(Some(5));
    overlay.call(&rpc, open("r2", "/p/a"), t0);
    assert_eq!(rpc.count(REVIEW_LUA), 2);

    // A connection that ends under a sent call.
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(OPEN_AND_SHOW_LUA, Err(RpcError::Closed));
    let out = overlay.tick(&rpc, t0);
    assert_eq!(
        out,
        vec![OverlayOutcome::Answered {
            request_id: "r2".to_owned(),
            result: Err("the connection to nvim closed".to_owned()),
        }]
    );
    // And an editor's own error text comes through as it is.
    overlay.call(&rpc, open("r3", "/p/a"), t0);
    assert_eq!(rpc.count(REVIEW_LUA), 3, "a closed connection forgets the install");
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(OPEN_AND_SHOW_LUA, Err(RpcError::Nvim("E5108: boom".to_owned())));
    let out = overlay.tick(&rpc, t0);
    assert_eq!(
        out,
        vec![OverlayOutcome::Answered {
            request_id: "r3".to_owned(),
            result: Err("E5108: boom".to_owned()),
        }]
    );
}

#[test]
fn an_open_answers_with_what_the_editor_said() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, open("a", "/p/a"), t0);
    overlay.call(&rpc, open("b", "/p/b"), t0);
    overlay.call(&rpc, open("c", "/p/c"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    // One open in flight at a time: each answer lets the next go out.
    let mut out = Vec::new();
    rpc.answer_oldest_of(
        OPEN_AND_SHOW_LUA,
        Ok(map(vec![
            ("opened", Value::from(true)),
            ("drawn", Value::from(1)),
            ("skipped", Value::from(2)),
            ("notice", Value::from("2 hunks no longer match this buffer")),
            ("active", Value::from(1)),
        ])),
    );
    out.extend(overlay.tick(&rpc, t0));
    rpc.answer_oldest_of(
        OPEN_AND_SHOW_LUA,
        Ok(map(vec![
            ("opened", Value::from(false)),
            ("error", Value::from("the file does not exist")),
            ("active", Value::from(1)),
        ])),
    );
    out.extend(overlay.tick(&rpc, t0));
    rpc.answer_oldest_of(
        OPEN_AND_SHOW_LUA,
        Ok(map(vec![("opened", Value::from(true)), ("active", Value::from(1))])),
    );
    out.extend(overlay.tick(&rpc, t0));
    assert_eq!(rpc.count(OPEN_AND_SHOW_LUA), 3);
    let results: Vec<_> = out
        .iter()
        .map(|o| match o {
            OverlayOutcome::Answered { request_id, result } => (request_id.as_str(), result.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("a", Ok("opened; 2 hunks no longer match this buffer".to_owned())),
            ("b", Err("the file does not exist".to_owned())),
            ("c", Ok("opened".to_owned())),
        ]
    );
    assert_eq!(
        overlay.active_paths().iter().collect::<Vec<_>>(),
        vec![Path::new("/p/a")]
    );
}

#[test]
fn editor_lost_drops_everything() {
    let rpc = FakeEditor::new();
    let mut overlay = EditorOverlay::new(Owner::Embedded);
    let t0 = Instant::now();
    overlay.call(&rpc, show("/p/a"), t0);
    rpc.answer(REVIEW_LUA, installed());
    overlay.tick(&rpc, t0);
    rpc.answer(SHOW_LUA, answered_show(1, 1));
    overlay.tick(&rpc, t0);
    assert_eq!(rpc.count(TAKE_EVENTS_LUA), 1, "a drain is in flight");
    overlay.call(&rpc, open("r1", "/p/b"), t0);
    overlay.call(&rpc, clear("/p/c"), t0);
    assert!(overlay.wants_ticks());

    overlay.editor_lost();
    assert!(overlay.active_paths().is_empty());
    assert!(!overlay.wants_ticks(), "no pending answer, no queue, nothing drawn");

    // Whatever the old editor says now is nobody's.
    rpc.answer(
        TAKE_EVENTS_LUA,
        events_answer(vec![revert_event("/p/a", 1, "x", "y")], 0),
    );
    let sent_before = rpc.sent.borrow().len();
    assert!(overlay.tick(&rpc, ms(t0, 5_000)).is_empty());
    assert_eq!(rpc.sent.borrow().len(), sent_before, "and nothing more is sent");

    overlay.call(&rpc, clear("/p/d"), ms(t0, 6_000));
    assert_eq!(rpc.codes().last(), Some(&REVIEW_LUA), "the next call installs afresh");
}

// ---- the real editor ----------------------------------------------------------------------

fn numbered(lines: &[&str]) -> Vec<u8> {
    lines.iter().flat_map(|l| format!("{l}\n").into_bytes()).collect()
}

fn lines_of(file: &[u8], start: u32, len: u32) -> Vec<Vec<u8>> {
    let lines: Vec<Vec<u8>> = file.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let from = (start.max(1) - 1) as usize;
    lines[from..from + len as usize].to_vec()
}

/// Waits (polling, as the panel's tick does) until `want` outcomes arrived or five seconds passed.
fn collect(
    overlay: &mut EditorOverlay,
    rpc: &dyn EditorRpc,
    what: &str,
    mut done: impl FnMut(&[OverlayOutcome]) -> bool,
) -> Vec<OverlayOutcome> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        all.extend(overlay.tick(rpc, Instant::now()));
        if done(&all) {
            return all;
        }
        assert!(Instant::now() < deadline, "{what}: only {all:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn round_trip_with_real_nvim() {
    let dir = std::env::temp_dir().join(format!("rev-drv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("files")).unwrap();
    let sock = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
    let home = dir.join("nvim");
    let mut nvim = embed_client::Embed::start(
        &home,
        &["--listen", sock.to_str().unwrap()],
        &[("HOME", home.to_str().unwrap())],
    );
    nvim.lua("vim.g.maplocalleader = ','", vec![]);
    let connect_by = Instant::now() + Duration::from_secs(5);
    let link: NvimLink = loop {
        match NvimLink::connect(&sock, Duration::from_secs(1)) {
            Ok((link, _events)) => break link,
            Err(e) => {
                assert!(Instant::now() < connect_by, "could not connect: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    let rpc: &dyn EditorRpc = &link;

    // Line 3 became X3 on disk; the hunk is the one line.
    let base = numbered(&["a1", "a2", "a3", "a4", "a5"]);
    let end = numbered(&["a1", "a2", "X3", "a4", "a5"]);
    let path = dir.join("files").canonicalize().unwrap().join("f.txt");
    std::fs::write(&path, &end).unwrap();
    let hunk = ShowHunk::from_file_lines(1, (3, 1), (3, 1), &lines_of(&base, 3, 1), &lines_of(&end, 3, 1));

    let mut overlay = EditorOverlay::new(Owner::Embedded);
    overlay.call(
        rpc,
        OverlayCall::OpenAndShow {
            request_id: "r1".to_owned(),
            path: path.clone(),
            line: Some(3),
            meta: meta(),
            hunks: Some(vec![hunk.clone()]),
        },
        Instant::now(),
    );
    let out = collect(&mut overlay, rpc, "the open's answer", |o| !o.is_empty());
    assert_eq!(
        out,
        vec![OverlayOutcome::Answered {
            request_id: "r1".to_owned(),
            result: Ok("opened".to_owned())
        }]
    );
    assert!(overlay.active_paths().contains(&path));

    // The user reverts the hunk under the cursor.
    nvim.input(",r");
    let out = collect(&mut overlay, rpc, "the revert event", |o| !o.is_empty());
    assert_eq!(out.len(), 1, "{out:?}");
    match &out[0] {
        OverlayOutcome::Reverts(reverts) => {
            assert_eq!(reverts.len(), 1);
            assert_eq!(reverts[0].meta, meta());
            assert_eq!(reverts[0].path, path.to_str().unwrap());
            assert_eq!(reverts[0].hunk.id, 1);
            assert_eq!(reverts[0].hunk.old_bytes(), b"a3\n");
            assert_eq!(reverts[0].hunk.new_bytes(), b"X3\n");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        end,
        "the buffer changed, the file did not"
    );

    // Clearing takes the review off; a clear of what is not drawn is harmless.
    overlay.call(rpc, OverlayCall::ClearAll, Instant::now());
    let cleared_by = Instant::now() + Duration::from_secs(5);
    while !overlay.active_paths().is_empty() {
        overlay.tick(rpc, Instant::now());
        assert!(Instant::now() < cleared_by, "the clear never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        nvim.lua("return rawget(_G, '__eitri_review').take_events().active", vec![]),
        Value::from(0),
        "nothing is drawn any more"
    );
    drop(nvim);
    let _ = std::fs::remove_dir_all(&dir);
}
