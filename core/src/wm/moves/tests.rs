//! Moves over the fake host and a recording link: one window as the source and one as the destination, each a
//! controller behind its own driver, with the link replaced by the port and the other window's answers posted as
//! peer events.

use super::*;
use crate::keymap::Action;
use crate::layout::{Axis, Layout, ModuleId, Node, Size, Workspaces};
use crate::window_link::moves::{MoveId, MOVE_DEADLINE};
use crate::window_link::{FocusTarget, Msg, Shape};
use crate::wm::fake::{FakeHost, FakePeerPort, FakeScheduler, Logged, SentMsg};
use crate::wm::module::ModuleState;
use crate::wm::verbs::rig::decl;
use crate::wm::*;
use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

const WIN: WinId = WinId::MAIN;
const PEER: PeerId = PeerId(2);
const GRID: Size = Size { w: 1200, h: 800 };

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn terminal2() -> ModuleId {
    ModuleId::parse("terminal#2").unwrap()
}

/// One window of the pair behind its driver, linked to the other through the recording port.
struct End {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    port: Rc<FakePeerPort>,
    layout: Rc<RefCell<Layout>>,
    workspaces: Rc<RefCell<Workspaces>>,
}

fn open(layout: Layout, role: WindowRole, presence: EditorPresence) -> End {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let mut workspaces = Workspaces::single();
    // A second workspace holding the same modules, so "every workspace" is more than the one on screen.
    workspaces.create("2", layout.clone()).unwrap();
    let layout = Rc::new(RefCell::new(layout));
    let workspaces = Rc::new(RefCell::new(workspaces));
    controller.open_window(
        WIN,
        WindowSpec {
            role,
            presence,
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: workspaces.clone(),
            saver: None,
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    let port = FakePeerPort::new();
    driver.attach_peer(WIN, PEER, port.clone());
    End {
        driver,
        host,
        scheduler,
        port,
        layout,
        workspaces,
    }
}

/// Window 1: `editor | (agent / terminal)`, the keys on the terminal.
fn source() -> End {
    let layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        ),
        terminal(),
    )
    .unwrap();
    let end = open(layout, WindowRole::Main, EditorPresence::Owned);
    end.host.set_module_size(terminal(), Size { w: 600, h: 400 });
    end
}

/// Window 2 holding the chat alone.
fn destination() -> End {
    let layout = Layout::new(Node::Leaf(agent()), agent()).unwrap();
    open(
        layout,
        WindowRole::Linked { side: LinkSide::Second },
        EditorPresence::Proxy,
    )
}

/// Window 2 holding the chat and a terminal of its own, so a terminal arriving takes another number.
fn destination_with_a_terminal() -> End {
    let layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        agent(),
    )
    .unwrap();
    open(
        layout,
        WindowRole::Linked { side: LinkSide::Second },
        EditorPresence::Proxy,
    )
}

fn pipe_fd() -> OwnedFd {
    let (r, _w) = std::io::pipe().unwrap();
    r.into()
}

impl End {
    fn run(&self, action: Action) {
        self.driver.run(WIN, action, Origin::Key { keycode: 7 });
    }

    fn move_terminal(&self, follow: bool) {
        self.run(Action::MoveToWindow { module: None, follow });
    }

    fn peer(&self, event: PeerEvent) {
        self.driver.post(HostEvent::Peer(event));
    }

    fn ready(&self, id: u64) {
        self.peer(PeerEvent::MoveReady {
            peer: PEER,
            id: MoveId(id),
        });
    }

    fn done(&self, id: u64) {
        self.peer(PeerEvent::MoveDone {
            peer: PEER,
            id: MoveId(id),
        });
    }

    fn detached(&self, id: u64, record: serde_json::Value, handoff: Handoff) {
        self.driver.post(HostEvent::Terminal(TermEvent::Detached {
            win: WIN,
            module: terminal(),
            mv: MoveId(id),
            record,
            handoff,
        }));
    }

    fn offer(&self, id: u64, module: ModuleId, shape: Shape) {
        self.peer(PeerEvent::MoveOffer {
            peer: PEER,
            id: MoveId(id),
            module,
            kind: MoveKind::Terminal,
            shape,
            size: (600, 400),
            session: None,
        });
    }

    fn deliver(&self, id: u64, fds: Descriptors) {
        self.peer(PeerEvent::Move {
            peer: PEER,
            id: MoveId(id),
            record: serde_json::json!({ "screen": "moved" }),
            fds,
        });
    }

    fn woke(&self) {
        self.peer(PeerEvent::Woke { peer: PEER });
    }

    fn link_closed(&self) {
        self.peer(PeerEvent::LinkClosed { peer: PEER });
    }

    /// The ops applied so far, but the chrome's announcements, which the chrome's own tests count.
    fn ops(&self) -> Vec<HostOp> {
        self.host
            .log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, _) if !matches!(op, HostOp::Chrome { .. }) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn ops_since(&self, mark: usize) -> Vec<HostOp> {
        self.ops().into_iter().skip(mark).collect()
    }

    fn mark(&self) -> usize {
        self.ops().len()
    }

    fn toast(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).toast.map(|toast| toast.text))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    fn away(&self, module: &ModuleId) -> bool {
        self.layout.borrow().is_away(module)
    }

    fn state(&self, module: &ModuleId) -> Option<ModuleState> {
        self.driver
            .query(|c| c.lifecycle(WIN).and_then(|l| l.state(module)).cloned())
    }

    fn sent(&self) -> Vec<SentMsg> {
        self.port.sent()
    }

    fn last_sent(&self) -> Msg {
        self.sent().last().expect("something was sent").msg.clone()
    }
}

fn detach(module: ModuleId, id: u64) -> HostOp {
    HostOp::Terminal(TermOp::Detach {
        win: WIN,
        module,
        mv: MoveId(id),
    })
}
fn resume(module: ModuleId) -> HostOp {
    HostOp::Terminal(TermOp::Resume { win: WIN, module })
}
fn forget(module: ModuleId) -> HostOp {
    HostOp::Terminal(TermOp::Forget { win: WIN, module })
}

fn is_adopt(op: &HostOp) -> bool {
    matches!(op, HostOp::AdoptMoved { .. })
}

fn handoff() -> Handoff {
    Handoff::new(Rc::new(7u32))
}

/// The source's terminal offered, frozen and sent as move 1.
fn sent_move(src: &End) {
    src.move_terminal(false);
    src.ready(1);
    src.detached(1, serde_json::json!({ "screen": "x" }), handoff());
    assert_eq!(
        src.port.kinds(),
        vec!["move_offer", "move"],
        "the premise: move 1 is sent"
    );
}

// The source's side.

#[test]
fn a_peer_move_keeps_an_away_placeholder_until_move_done() {
    let src = source();
    sent_move(&src);
    assert!(!src.away(&terminal()), "before move_done the module is here");
    assert!(src.layout.borrow().is_shown(&terminal()), "and shown where it was");
    assert!(
        !src.ops().contains(&forget(terminal())),
        "nothing dropped before the commit"
    );

    let mark = src.mark();
    src.done(1);
    let ops = src.ops_since(mark);
    assert!(ops.contains(&forget(terminal())), "the local copy goes: {ops:#?}");
    assert!(src.away(&terminal()), "away in the live workspace");
    assert!(
        src.layout.borrow().contains(&terminal()),
        "its leaf is still in the tree"
    );
    let workspaces = src.workspaces.borrow();
    let stored = workspaces.stored("2").expect("the second workspace");
    assert!(
        stored.is_away(&terminal()) && stored.contains(&terminal()),
        "away in every workspace, leaf kept"
    );
    drop(workspaces);
    assert_eq!(src.state(&terminal()), Some(ModuleState::Away));
    // It held the keys: they go to another module before its host leaves the screen.
    let focus = ops
        .iter()
        .position(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m != terminal()))
        .expect("the keys go elsewhere");
    let unmap = ops
        .iter()
        .position(|op| {
            *op == HostOp::Unmap {
                win: WIN,
                module: terminal(),
            }
        })
        .expect("the terminal leaves the screen");
    assert!(focus < unmap, "the keys move before the unmap: {ops:#?}");
}

#[test]
fn the_source_sends_offer_then_freezes_only_after_ready() {
    let src = source();
    src.move_terminal(false);
    assert_eq!(
        src.sent().iter().map(|s| s.msg.clone()).collect::<Vec<_>>(),
        vec![Msg::MoveOffer {
            mv: 1,
            module: "terminal".into(),
            kind: "terminal".into(),
            shape: Shape::Tiled,
            size: (600, 400),
            session: None,
        }]
    );
    assert!(
        !src.ops().contains(&detach(terminal(), 1)),
        "nothing frozen before move_ready"
    );

    src.ready(1);
    assert!(
        src.ops().contains(&detach(terminal(), 1)),
        "frozen once the destination is ready"
    );
    assert_eq!(src.port.kinds(), vec!["move_offer"], "move waits for the record");

    let record = serde_json::json!({ "screen": "rows", "cols": 80 });
    src.detached(1, record.clone(), handoff());
    assert_eq!(src.port.kinds(), vec!["move_offer", "move"]);
    assert_eq!(src.last_sent(), Msg::Move { mv: 1, record });
}

#[test]
fn a_float_is_offered_by_its_rectangle_relative_to_the_grid() {
    let layout = Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
        vec![crate::layout::Float {
            id: terminal(),
            rect: crate::layout::FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 600,
                h: 400,
            },
            scratch: false,
        }],
        Default::default(),
        terminal(),
        Default::default(),
    )
    .unwrap();
    let src = open(layout, WindowRole::Main, EditorPresence::Owned);
    src.move_terminal(false);
    let Msg::MoveOffer { shape, .. } = src.last_sent() else {
        panic!("an offer");
    };
    assert_eq!(
        shape,
        Shape::Float {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5
        }
    );
}

#[test]
fn a_failed_peer_move_restores_the_module_where_it_was() {
    let src = source();
    sent_move(&src);
    let mark = src.mark();
    src.peer(PeerEvent::MoveAbort {
        peer: PEER,
        id: MoveId(1),
        why: "no room in window 2".into(),
    });
    assert_eq!(
        src.ops_since(mark)
            .iter()
            .filter(|op| **op == resume(terminal()))
            .count(),
        1
    );
    assert!(!src.away(&terminal()) && src.layout.borrow().is_shown(&terminal()));
    assert_eq!(src.toast().as_deref(), Some("no room in window 2"));
    let mark = src.mark();
    src.done(1);
    assert!(
        !src.ops_since(mark).contains(&forget(terminal())),
        "a late move_done changes nothing"
    );
    assert!(!src.away(&terminal()));
}

#[test]
fn a_refused_offer_resumes_without_a_freeze() {
    let src = source();
    src.move_terminal(false);
    src.peer(PeerEvent::MoveRefused {
        peer: PEER,
        id: MoveId(1),
        why: "window 2 is full".into(),
    });
    let ops = src.ops();
    assert!(
        !ops.contains(&detach(terminal(), 1)) && !ops.contains(&resume(terminal())),
        "{ops:#?}"
    );
    assert_eq!(src.toast().as_deref(), Some("window 2 is full"));
    src.ready(1);
    assert!(
        !src.ops().contains(&detach(terminal(), 1)),
        "a ready out of turn freezes nothing"
    );
}

#[test]
fn an_offer_past_its_deadline_resumes_and_a_sent_move_only_says_not_answering() {
    let src = source();
    src.move_terminal(false);
    src.scheduler.advance(MOVE_DEADLINE - Duration::from_millis(1));
    assert_eq!(src.toast(), None, "not yet");
    src.scheduler.advance(Duration::from_millis(1));
    assert_eq!(src.toast().as_deref(), Some("the move took too long"));
    assert!(!src.ops().contains(&resume(terminal())), "an offer froze nothing");
    assert_eq!(
        src.last_sent(),
        Msg::MoveAbort {
            mv: 1,
            why: "the move took too long".into(),
        },
        "the other window is told, so it drops the place it may have made"
    );
    // That window, having accepted the offer, drops what it prepared for it when it reads the abort.
    let dst = destination();
    dst.offer(1, terminal(), Shape::Tiled);
    dst.peer(PeerEvent::MoveAbort {
        peer: PEER,
        id: MoveId(1),
        why: "the move took too long".into(),
    });
    assert!(dst.ops().contains(&HostOp::DropSurface {
        win: WIN,
        module: terminal(),
        why: DropWhy::MoveGivenUp,
    }));

    // Move 3: sent, then nothing heard.
    src.move_terminal(false);
    src.ready(3);
    src.detached(3, serde_json::Value::Null, handoff());
    assert_eq!(src.port.kinds().last(), Some(&"move"));
    let mark = src.mark();
    src.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(src.toast().as_deref(), Some("window 2 is not answering"));
    src.scheduler.advance(Duration::from_secs(3600));
    let said = src.notes().iter().filter(|n| n.contains("is not answering")).count();
    assert_eq!(said, 1, "the note comes once");
    assert!(
        !src.ops_since(mark).contains(&resume(terminal())),
        "a sent move stays frozen"
    );
    assert!(!src.away(&terminal()));
    src.done(3);
    assert!(src.away(&terminal()), "a late commit still counts");
}

#[test]
fn a_frozen_move_past_its_deadline_resumes_and_tells_the_destination() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    assert!(src.ops().contains(&detach(terminal(), 1)), "the premise: frozen");
    let mark = src.mark();
    src.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(src.toast().as_deref(), Some("the move took too long"));
    assert!(
        src.ops_since(mark).contains(&resume(terminal())),
        "the pane reads again"
    );
    assert_eq!(src.port.kinds(), vec!["move_offer", "move_abort"]);
    assert!(!src.away(&terminal()));
}

#[test]
fn a_repeated_freeze_answer_never_resumes_a_sent_move() {
    let src = source();
    sent_move(&src);
    let mark = src.mark();
    src.detached(1, serde_json::Value::Null, handoff());
    assert!(
        !src.ops_since(mark).contains(&resume(terminal())),
        "the other window may hold the descriptors already: this one must not read them too"
    );
    assert_eq!(src.port.kinds(), vec!["move_offer", "move"], "nothing more is sent");
    src.done(1);
    assert!(src.away(&terminal()), "the sent move still commits");
}

#[test]
fn a_late_freeze_answer_for_an_expired_move_is_never_sent_as_its_retrys() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    src.scheduler.advance(MOVE_DEADLINE);
    assert!(
        src.ops().contains(&resume(terminal())),
        "the premise: move 1 expired frozen"
    );
    src.move_terminal(false);
    src.ready(3);
    assert!(
        src.ops().contains(&detach(terminal(), 3)),
        "the premise: the retry froze the module again"
    );
    let sent_before = src.port.kinds();
    let mark = src.mark();

    src.detached(1, serde_json::json!({ "screen": "before the resume" }), handoff());
    assert_eq!(
        src.port.kinds(),
        sent_before,
        "the expired move's snapshot is not sent for the retry"
    );
    assert!(
        !src.ops_since(mark).contains(&resume(terminal())),
        "the retry's freeze stays: reading again would let both windows run the session"
    );

    let fresh = handoff();
    src.detached(3, serde_json::json!({ "screen": "now" }), fresh.clone());
    let sent = src.sent();
    let last = sent.last().expect("move went out");
    assert_eq!(
        last.msg,
        Msg::Move {
            mv: 3,
            record: serde_json::json!({ "screen": "now" }),
        },
        "the retry carries its own record"
    );
    let passed = last.handoff.clone().expect("with a handoff");
    assert!(Rc::ptr_eq(passed.inner(), fresh.inner()), "and its own handoff");
}

#[test]
fn a_freeze_answer_for_a_move_that_ended_resumes_when_no_other_move_holds_the_module() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    src.scheduler.advance(MOVE_DEADLINE);
    let mark = src.mark();
    src.detached(1, serde_json::Value::Null, handoff());
    assert!(
        src.ops_since(mark).contains(&resume(terminal())),
        "a freeze nothing waits for is undone"
    );
    assert_eq!(
        src.port.kinds(),
        vec!["move_offer", "move_abort"],
        "and nothing is sent"
    );
}

#[test]
fn a_moving_module_is_not_killed_until_its_move_ends() {
    let src = source();
    sent_move(&src);
    let mark = src.mark();
    src.run(Action::ModuleKill);
    src.driver.kill_confirmed(WIN, &terminal());
    let ops = src.ops_since(mark);
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::HangUp { .. }))),
        "the session the other window is taking over is not ended here: {ops:#?}"
    );
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::MOVING_TEXT));
    assert!(src.layout.borrow().is_shown(&terminal()));
    src.done(1);
    assert!(src.away(&terminal()), "the move still commits");
}

#[test]
fn a_local_move_is_refused_with_one_reason() {
    let src = source();
    let plans = Rc::new(RefCell::new(Vec::new()));
    let out = plans.clone();
    src.driver.test_batch(move |c| {
        let w = c.window_mut(WIN).unwrap();
        for module in [editor(), agent(), terminal()] {
            out.borrow_mut().push(plan(w, &module, &MoveTarget::Local(WinId(2))));
        }
        Batch::default()
    });
    assert_eq!(
        *plans.borrow(),
        vec![
            MovePlan::Refused {
                why: crate::wm::feedback::LOCAL_MOVE_TEXT
            };
            3
        ]
    );
}

#[test]
fn the_editor_and_an_away_module_are_never_offered() {
    let src = source();
    src.run(Action::MoveToWindow {
        module: Some(editor()),
        follow: false,
    });
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::EDITOR_MOVE_TEXT));
    src.layout.borrow_mut().set_away(&agent(), true).unwrap();
    src.run(Action::MoveToWindow {
        module: Some(agent()),
        follow: false,
    });
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::NO_SECOND_WINDOW_TEXT));
    assert!(src.sent().is_empty(), "nothing offered");

    // A chat is refused until the other window can take one; a terminal on its way is not offered twice.
    src.layout.borrow_mut().set_away(&agent(), false).unwrap();
    src.run(Action::MoveToWindow {
        module: Some(agent()),
        follow: false,
    });
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::NO_SECOND_WINDOW_TEXT));
    src.move_terminal(false);
    src.move_terminal(false);
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::MOVING_TEXT));
    assert_eq!(src.port.kinds(), vec!["move_offer"]);
}

#[test]
fn a_terminal_handoff_is_passed_to_the_link_unread() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    let given = handoff();
    src.detached(1, serde_json::Value::Null, given.clone());
    let sent = src.sent();
    let passed = sent
        .last()
        .and_then(|s| s.handoff.clone())
        .expect("move carries the handoff");
    assert!(Rc::ptr_eq(passed.inner(), given.inner()), "the very handoff");
    assert_eq!(
        sent.iter().filter(|s| s.handoff.is_some()).count(),
        1,
        "only move carries one"
    );
}

#[test]
fn a_refused_freeze_gives_the_move_up_before_anything_left() {
    let src = source();
    src.host
        .refuse_op(|op| matches!(op, HostOp::Terminal(TermOp::Detach { .. })));
    src.move_terminal(false);
    src.ready(1);
    assert_eq!(src.port.kinds(), vec!["move_offer", "move_abort"]);
    assert!(src.ops().contains(&resume(terminal())), "the pane reads again");
    assert!(!src.away(&terminal()));
}

#[test]
fn a_move_that_cannot_be_queued_is_given_up_and_the_pane_resumed() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    src.port.refuse_sends(Some("the link is closing"));
    src.detached(1, serde_json::Value::Null, handoff());
    src.port.refuse_sends(None);
    assert!(src.ops().contains(&resume(terminal())));
    assert_eq!(src.toast().as_deref(), Some("the link is closing"));
    src.done(1);
    assert!(!src.away(&terminal()), "never sent, never committed");
}

// The destination's side.

#[test]
fn the_receiving_window_prepares_before_it_maps() {
    let dst = destination();
    let port = dst.port.clone();
    let sent_at_prepare = Rc::new(Cell::new(None));
    let seen = sent_at_prepare.clone();
    dst.host.on_apply(move |_, _, op| {
        if matches!(op, HostOp::PrepareSurface { .. }) {
            seen.set(Some(port.sent().len()));
        }
    });
    dst.offer(1, terminal(), Shape::Tiled);
    assert!(dst.ops().contains(&HostOp::PrepareSurface {
        win: WIN,
        module: terminal(),
        shape: Shape::Tiled,
        size: (600, 400),
    }));
    assert_eq!(sent_at_prepare.get(), Some(0), "prepared before move_ready went out");
    assert_eq!(dst.port.kinds(), vec!["move_ready"]);
    assert!(
        !dst.ops().contains(&HostOp::Map {
            win: WIN,
            module: terminal()
        }),
        "made, not mapped"
    );

    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    assert_eq!(dst.port.kinds(), vec!["move_ready", "move_done"]);
    assert!(dst.sent()[1].confirmed, "move_done is a confirmed send");
    assert!(
        !dst.ops().iter().any(is_adopt),
        "nothing built before move_done is written"
    );

    dst.port.answer(0, Ok(()));
    dst.woke();
    let ops = dst.ops();
    let adopt = ops.iter().position(is_adopt).expect("adopted once written");
    let map = ops
        .iter()
        .position(|op| {
            *op == HostOp::Map {
                win: WIN,
                module: terminal(),
            }
        })
        .expect("mapped after");
    assert!(adopt < map, "{ops:#?}");
    assert!(dst.layout.borrow().is_shown(&terminal()));
    assert_eq!(dst.layout.borrow().focus(), &agent(), "the keys stay where they were");
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::Start { .. }))),
        "a moved shell runs already"
    );
}

#[test]
fn the_destination_commits_only_after_move_done_is_written() {
    let dst = destination();
    dst.offer(1, terminal(), Shape::Tiled);
    let fds = Descriptors::new(vec![pipe_fd()]);
    dst.deliver(1, fds.clone());
    dst.woke();
    assert!(!dst.ops().iter().any(is_adopt), "queued, not written: nothing");
    dst.port.answer(0, Ok(()));
    dst.woke();
    let adopted: Vec<HostOp> = dst.ops().into_iter().filter(is_adopt).collect();
    assert_eq!(adopted.len(), 1);
    let HostOp::AdoptMoved { fds: given, record, .. } = &adopted[0] else {
        unreachable!()
    };
    assert_eq!(given, &fds, "the very descriptors");
    assert_eq!(given.len(), 1);
    assert_eq!(record, &serde_json::json!({ "screen": "moved" }));
    dst.woke();
    assert_eq!(dst.ops().into_iter().filter(is_adopt).count(), 1, "committed once");

    let dst = destination();
    dst.offer(1, terminal(), Shape::Tiled);
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    dst.port
        .answer(0, Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
    dst.woke();
    let ops = dst.ops();
    assert!(!ops.iter().any(is_adopt), "not written: never built");
    assert!(ops.contains(&HostOp::DropSurface {
        win: WIN,
        module: terminal(),
        why: DropWhy::MoveGivenUp,
    }));
}

#[test]
fn a_second_move_for_a_received_move_leaves_the_first_to_commit() {
    let dst = destination();
    dst.offer(1, terminal(), Shape::Tiled);
    let fds = Descriptors::new(vec![pipe_fd()]);
    dst.deliver(1, fds.clone());
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    assert_eq!(
        dst.port.kinds(),
        vec!["move_ready", "move_done"],
        "no abort: the source would take the module back while this window commits it"
    );
    assert!(!dst.ops().iter().any(|op| matches!(op, HostOp::DropSurface { .. })));
    dst.port.answer(0, Ok(()));
    dst.woke();
    let adopted: Vec<HostOp> = dst.ops().into_iter().filter(is_adopt).collect();
    assert_eq!(adopted.len(), 1);
    let HostOp::AdoptMoved { fds: given, .. } = &adopted[0] else {
        unreachable!()
    };
    assert_eq!(given, &fds, "built from the first move's descriptors");
}

#[test]
fn a_new_instance_never_takes_an_arriving_modules_number() {
    let dst = destination_with_a_terminal();
    dst.offer(1, terminal(), Shape::Tiled);
    assert!(dst.ops().iter().any(|op| matches!(
        op,
        HostOp::PrepareSurface { module, .. } if *module == terminal2()
    )));
    let mark = dst.mark();
    dst.run(Action::InstanceNew {
        kind: crate::layout::ModuleKind::Terminal,
    });
    let created: Vec<ModuleId> = dst
        .ops_since(mark)
        .into_iter()
        .filter_map(|op| match op {
            HostOp::CreateSurface { module, .. } => Some(module),
            _ => None,
        })
        .collect();
    assert_eq!(created, vec![ModuleId::parse("terminal#3").unwrap()]);

    let mark = dst.mark();
    dst.run(Action::ScratchpadShow {
        module: Some(terminal2()),
    });
    assert!(
        !dst.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::CreateSurface { .. })),
        "the arriving number is not made here"
    );
}

#[test]
fn an_arrival_never_takes_a_number_a_later_builds_kept_data_names() {
    let dst = destination();
    let mut kept = crate::layout::workspace::Kept::default();
    kept.top.insert(
        "future_overlays".into(),
        serde_json::json!([{ "module": "terminal#2" }]),
    );
    dst.workspaces.borrow_mut().retake_kept(kept);
    assert_eq!(
        dst.workspaces.borrow().reserved(),
        vec![terminal2()],
        "the premise: terminal#2 is reserved"
    );
    dst.offer(1, terminal2(), Shape::Tiled);
    let prepared: Vec<ModuleId> = dst
        .ops()
        .into_iter()
        .filter_map(|op| match op {
            HostOp::PrepareSurface { module, .. } => Some(module),
            _ => None,
        })
        .collect();
    assert_eq!(prepared, vec![ModuleId::parse("terminal#3").unwrap()]);
}

#[test]
fn a_refused_prepare_refuses_the_offer() {
    let dst = destination();
    dst.host.refuse_op(|op| matches!(op, HostOp::PrepareSurface { .. }));
    dst.offer(1, terminal(), Shape::Tiled);
    assert_eq!(dst.port.kinds(), vec!["move_refused"]);
    let why = match dst.last_sent() {
        Msg::MoveRefused { mv: 1, why } => why,
        other => panic!("{other:?}"),
    };
    assert!(!dst
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Map { module, .. } if *module == terminal())));

    // The source of the same script keeps its terminal, frozen never.
    let src = source();
    src.move_terminal(false);
    src.peer(PeerEvent::MoveRefused {
        peer: PEER,
        id: MoveId(1),
        why: why.clone(),
    });
    let ops = src.ops();
    assert!(!ops.contains(&detach(terminal(), 1)) && !ops.contains(&resume(terminal())));
    assert_eq!(src.toast(), Some(why));
}

#[test]
fn the_link_ending_mid_move_restores_the_module_and_commits_nothing() {
    // After the offer: nothing was frozen, so nothing resumes; the module is here.
    let src = source();
    src.move_terminal(false);
    src.link_closed();
    assert_eq!(src.toast().as_deref(), Some("the other window closed"));
    assert!(!src.ops().contains(&resume(terminal())));
    assert!(!src.away(&terminal()));
    src.done(1);
    assert!(
        !src.ops().contains(&forget(terminal())) && !src.away(&terminal()),
        "a late move_done"
    );

    // After `move`: the frozen pane reads again.
    let src = source();
    sent_move(&src);
    src.link_closed();
    assert!(src.ops().contains(&resume(terminal())));
    assert!(!src.away(&terminal()) && src.layout.borrow().is_shown(&terminal()));
    src.done(1);
    assert!(
        !src.ops().contains(&forget(terminal())) && !src.away(&terminal()),
        "a late move_done"
    );

    // The destination, before its commit: what it prepared goes and nothing is built.
    let dst = destination();
    dst.offer(1, terminal(), Shape::Tiled);
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    dst.link_closed();
    let ops = dst.ops();
    assert!(!ops.iter().any(is_adopt), "{ops:#?}");
    assert!(ops.contains(&HostOp::DropSurface {
        win: WIN,
        module: terminal(),
        why: DropWhy::MoveGivenUp,
    }));
    dst.port.answer(0, Ok(()));
    dst.woke();
    assert!(!dst.ops().iter().any(is_adopt), "nothing is committed later");
}

#[test]
fn a_followed_move_raises_the_peer_and_focuses_the_arrival() {
    let src = source();
    src.move_terminal(true);
    src.ready(1);
    src.detached(1, serde_json::Value::Null, handoff());
    let mark = src.mark();
    src.done(1);
    assert!(src
        .ops_since(mark)
        .contains(&HostOp::RaisePeer { win: WIN, peer: PEER }));
    src.peer(PeerEvent::Raised {
        peer: PEER,
        token: Some("tok".into()),
    });
    assert_eq!(
        src.last_sent(),
        Msg::Focus {
            target: FocusTarget::Module { id: "terminal".into() },
            token: Some("tok".into()),
        }
    );

    // The destination already holds a `terminal`: the arrival takes the next number, and the focus that names the
    // offered id lands on it.
    let dst = destination_with_a_terminal();
    dst.offer(1, terminal(), Shape::Tiled);
    assert!(dst.ops().iter().any(|op| matches!(
        op,
        HostOp::PrepareSurface { module, .. } if *module == terminal2()
    )));
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    // The confirmation is in, but this window was not woken for it before the focus came.
    dst.port.answer(0, Ok(()));
    let mark = dst.mark();
    dst.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Module { id: "terminal".into() },
        token: Some("tok".into()),
    });
    let ops = dst.ops_since(mark);
    assert!(ops.contains(&HostOp::Present {
        win: WIN,
        activation: Some(ActivationToken("tok".into())),
    }));
    assert!(
        ops.iter()
            .any(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == terminal2())),
        "{ops:#?}"
    );
    assert_eq!(dst.layout.borrow().focus(), &terminal2());
}

#[test]
fn window_other_raises_the_peer_and_gives_it_the_keys() {
    let src = source();
    src.run(Action::WindowOther);
    assert!(src.ops().contains(&HostOp::RaisePeer { win: WIN, peer: PEER }));
    src.peer(PeerEvent::Raised {
        peer: PEER,
        token: None,
    });
    assert_eq!(
        src.last_sent(),
        Msg::Focus {
            target: FocusTarget::Window,
            token: None,
        }
    );
}

#[test]
fn a_detached_peer_ends_its_moves_and_the_verbs_refuse_again() {
    let src = source();
    sent_move(&src);
    src.driver.detach_peer(WIN);
    assert!(src.ops().contains(&resume(terminal())));
    src.run(Action::WindowOther);
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::NO_SECOND_WINDOW_TEXT));
}
