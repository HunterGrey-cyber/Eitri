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
    open_with(layout, role, presence, None)
}

fn open_with(layout: Layout, role: WindowRole, presence: EditorPresence, page: Option<SurfaceDecl>) -> End {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let keymap = crate::keymap::Keymap::defaults();
    let modules = crate::layout::ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(modules));
    let mut hosted: Vec<_> = layout
        .modules()
        .iter()
        .map(|id| {
            page.as_ref()
                .filter(|d| d.module == *id)
                .cloned()
                .unwrap_or_else(|| decl(id))
        })
        .collect();
    if let Some(page) = page {
        if !hosted.iter().any(|decl| decl.module == page.module) {
            hosted.push(page);
        }
    }
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
        self.port
            .sent()
            .into_iter()
            .filter(|s| !matches!(s.msg, Msg::Summary(_)))
            .collect()
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.sent().iter().map(|s| s.msg.kind()).collect()
    }

    fn last_sent(&self) -> Msg {
        self.sent().last().expect("something was sent").msg.clone()
    }

    /// The y/n's answer, typed as its key.
    fn answer(&self, key: &str) {
        let spec = crate::keymap::KeySpec::parse(key).unwrap();
        self.driver.key(
            WIN,
            &KeyStage::Window,
            KeyPress {
                keycode: 42,
                press: crate::keymap::prefix::Press::Key(spec),
                key: Some(spec.key),
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
    }

    fn prompt(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).prompt.map(|prompt| prompt.text))
    }

    /// Move 1 of `terminal` offered, delivered and committed here: the arrival built with an `AdoptMoved`.
    fn committed_arrival(&self) {
        self.offer(1, terminal(), Shape::Tiled);
        self.deliver(1, Descriptors::new(vec![pipe_fd()]));
        self.port.answer(0, Ok(()));
        self.woke();
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
    assert_eq!(src.kinds(), vec!["move_offer", "move"], "the premise: move 1 is sent");
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

/// Where in `ops` the terminal's forget and the teardown's shutdown of every shell fall.
fn forget_and_shutdown(ops: &[HostOp]) -> (Option<usize>, Option<usize>) {
    (
        ops.iter().position(|op| *op == forget(terminal())),
        ops.iter()
            .position(|op| matches!(op, HostOp::Terminal(TermOp::ShutdownAll { .. }))),
    )
}

#[test]
fn a_plain_close_lets_a_sent_terminal_go_before_shutting_the_shells_down() {
    // The other window may have written `move_done` this window never reads: the shell is its own then, and the
    // teardown's hang-up of every shell would end it there.
    let src = source();
    sent_move(&src);
    let mark = src.mark();
    src.driver.close_requested(WIN, CloseBy::User);
    src.scheduler.advance(Duration::ZERO);
    let ops = src.ops_since(mark);
    assert!(
        ops.iter().any(|op| matches!(op, HostOp::CloseWindow { .. })),
        "the window closes: {ops:#?}"
    );
    let (forgot, shutdown) = forget_and_shutdown(&ops);
    let (forgot, shutdown) = (forgot.expect("the sent terminal is let go"), shutdown.unwrap());
    assert!(forgot < shutdown, "let go before the shells are hung up: {ops:#?}");
}

#[test]
fn an_end_signal_lets_a_sent_terminal_go_and_still_shuts_a_freezing_one_down() {
    let src = source();
    sent_move(&src);
    let mark = src.mark();
    src.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    let (forgot, shutdown) = forget_and_shutdown(&src.ops_since(mark));
    assert!(forgot.unwrap() < shutdown.unwrap());

    // Frozen but not yet sent: nothing left this window, and the shutdown ends the shell as any other.
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    let mark = src.mark();
    src.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    let (forgot, shutdown) = forget_and_shutdown(&src.ops_since(mark));
    assert_eq!(forgot, None, "a move not yet sent is not let go");
    assert!(shutdown.is_some());
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
    assert_eq!(src.kinds(), vec!["move_offer"], "move waits for the record");

    let record = serde_json::json!({ "screen": "rows", "cols": 80 });
    src.detached(1, record.clone(), handoff());
    assert_eq!(src.kinds(), vec!["move_offer", "move"]);
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
    assert_eq!(src.kinds().last(), Some(&"move"));
    let mark = src.mark();
    src.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(
        src.toast().as_deref(),
        Some("window 2 is not answering; prefix x closes it")
    );
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
    assert_eq!(src.kinds(), vec!["move_offer", "move_abort"]);
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
    assert_eq!(src.kinds(), vec!["move_offer", "move"], "nothing more is sent");
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
    let sent_before = src.kinds();
    let mark = src.mark();

    src.detached(1, serde_json::json!({ "screen": "before the resume" }), handoff());
    assert_eq!(
        src.kinds(),
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
    assert_eq!(src.kinds(), vec!["move_offer", "move_abort"], "and nothing is sent");
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
fn a_destination_silent_after_move_shows_not_answering_and_keeps_the_module_frozen() {
    let src = source();
    sent_move(&src);
    src.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(
        src.toast().as_deref(),
        Some("window 2 is not answering; prefix x closes it")
    );
    assert!(!src.ops().contains(&resume(terminal())), "still frozen");
    assert!(!src.away(&terminal()) && src.layout.borrow().is_shown(&terminal()));

    // The kill key asks about the window that stopped answering, not the module.
    let mark = src.mark();
    src.run(Action::ModuleKill);
    assert_eq!(src.prompt().as_deref(), Some("close window 2? (y/n)"));
    src.answer("n");
    assert_eq!(src.prompt(), None);
    assert!(
        !src.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::EndPeer { .. } | HostOp::Terminal(TermOp::HangUp { .. }))),
        "a no ends nothing"
    );

    src.run(Action::ModuleKill);
    src.answer("y");
    let ops = src.ops_since(mark);
    assert!(ops.contains(&HostOp::EndPeer { win: WIN, peer: PEER }), "{ops:#?}");
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::HangUp { .. }))),
        "the session the other window may hold is not ended here"
    );
    assert!(src.layout.borrow().is_shown(&terminal()), "the module stays");

    // That window's end gives the module back, reading again.
    src.link_closed();
    assert!(src.ops().contains(&resume(terminal())));
    assert!(!src.away(&terminal()) && src.layout.borrow().is_shown(&terminal()));
}

#[test]
fn the_question_about_ending_a_silent_window_names_what_ends_and_goes_with_that_window() {
    let src = source();
    sent_move(&src);
    src.peer(PeerEvent::Summary {
        peer: PEER,
        summary: crate::window_link::Summary {
            running: 2,
            ..Default::default()
        },
    });
    src.scheduler.advance(MOVE_DEADLINE);
    src.run(Action::ModuleKill);
    assert_eq!(
        src.prompt().as_deref(),
        Some("close window 2 (2 running end with it)? (y/n)")
    );
    // The window ends on its own while the question is up: a `y` would end whatever links as window 2 next.
    let mark = src.mark();
    src.link_closed();
    assert_eq!(src.prompt(), None, "the question went with the window it named");
    src.answer("y");
    assert!(
        !src.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::EndPeer { .. })),
        "a stale `y` ends nothing"
    );
}

#[test]
fn window_2_with_a_silent_window_1_says_how_the_module_comes_back_and_asks_nothing() {
    // Window 2 did not start window 1 and cannot end it: no question whose `y` would be refused.
    let layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        terminal(),
    )
    .unwrap();
    let src = open(
        layout,
        WindowRole::Linked { side: LinkSide::Second },
        EditorPresence::Proxy,
    );
    src.host.set_module_size(terminal(), Size { w: 600, h: 400 });
    src.move_terminal(false);
    let id = match src.last_sent() {
        Msg::MoveOffer { mv, .. } => mv,
        other => panic!("an offer first: {other:?}"),
    };
    src.ready(id);
    src.detached(id, serde_json::json!({ "screen": "x" }), handoff());
    assert_eq!(src.kinds(), vec!["move_offer", "move"], "the premise: the move is sent");
    src.scheduler.advance(MOVE_DEADLINE);
    let text = "window 1 is not answering; closing it gives the module back here";
    assert_eq!(src.toast().as_deref(), Some(text));
    let mark = src.mark();
    src.run(Action::ModuleKill);
    assert_eq!(src.prompt(), None);
    assert_eq!(src.toast().as_deref(), Some(text));
    src.answer("y");
    assert!(!src
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::EndPeer { .. } | HostOp::Terminal(TermOp::HangUp { .. }))));
}

#[test]
fn an_orphaned_window_2_says_its_editor_window_is_closed_and_how_to_start_it() {
    let dst = destination();
    dst.link_closed();
    dst.driver.detach_peer(WIN);
    for action in [
        Action::WindowOther,
        Action::MoveToWindow {
            module: None,
            follow: false,
        },
        Action::WorkspaceToWindow { follow: false },
    ] {
        dst.run(action.clone());
        assert_eq!(
            dst.toast().as_deref(),
            Some(crate::wm::feedback::ORPHAN_TEXT),
            "{action:?}"
        );
    }
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
    assert_eq!(
        src.kinds(),
        vec!["move_ask"],
        "the module the other window holds is asked for, not offered"
    );
    let src = source();

    // A chat whose host cannot say what would move is refused before anything is offered; a terminal on its way is
    // not offered twice.
    src.layout.borrow_mut().set_away(&agent(), false).unwrap();
    src.run(Action::MoveToWindow {
        module: Some(agent()),
        follow: false,
    });
    assert_eq!(src.toast().as_deref(), Some(NOT_CARRIED_OUT));
    src.move_terminal(false);
    src.move_terminal(false);
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::MOVING_TEXT));
    assert_eq!(src.kinds(), vec!["move_offer"]);
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
    assert_eq!(src.kinds(), vec!["move_offer", "move_abort"]);
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
            seen.set(Some(
                port.sent().iter().filter(|s| !matches!(s.msg, Msg::Summary(_))).count(),
            ));
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
    assert_eq!(dst.kinds(), vec!["move_ready"]);
    assert!(
        !dst.ops().contains(&HostOp::Map {
            win: WIN,
            module: terminal()
        }),
        "made, not mapped"
    );

    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    assert_eq!(dst.kinds(), vec!["move_ready", "move_done"]);
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
        dst.kinds(),
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
    assert_eq!(dst.kinds(), vec!["move_refused"]);
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
fn a_refused_adopt_takes_the_arrival_out_and_frees_its_name() {
    let dst = destination_with_a_terminal();
    dst.host.refuse_op(|op| matches!(op, HostOp::AdoptMoved { .. }));
    dst.committed_arrival();
    assert!(
        dst.ops().iter().any(is_adopt),
        "the premise: the adopt was asked and refused"
    );
    assert!(
        dst.toast()
            .is_some_and(|toast| toast.contains("could not be built here")),
        "{:?}",
        dst.toast()
    );
    assert!(!dst.layout.borrow().contains(&terminal2()), "out of the layout");
    let workspaces = dst.workspaces.borrow();
    assert!(
        !workspaces
            .stored("2")
            .expect("the second workspace")
            .contains(&terminal2()),
        "and of every workspace"
    );
    drop(workspaces);
    assert_eq!(dst.state(&terminal2()), None, "its lifecycle entry is gone");
    assert!(dst.ops().contains(&HostOp::DropSurface {
        win: WIN,
        module: terminal2(),
        why: DropWhy::MoveGivenUp,
    }));

    // A focus for the id the other window gave it finds the module that holds that id here, not the lost arrival.
    dst.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Module { id: "terminal".into() },
        token: None,
    });
    assert_eq!(dst.layout.borrow().focus(), &terminal());

    // Its number is free: the next new terminal takes it.
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
    assert_eq!(created, vec![terminal2()]);
}

#[test]
fn a_focus_for_a_module_hidden_here_shows_it_and_gives_it_the_keys() {
    let dst = destination();
    dst.committed_arrival();
    // The arrival is given the keys, then hidden.
    dst.run(Action::Module(terminal()));
    assert_eq!(dst.layout.borrow().focus(), &terminal(), "the premise: it has the keys");
    dst.run(Action::ModuleHide);
    assert!(
        !dst.layout.borrow().is_shown(&terminal()),
        "the premise: the arrival is hidden"
    );
    dst.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Module { id: "terminal".into() },
        token: None,
    });
    assert!(dst.layout.borrow().is_shown(&terminal()), "brought on screen");
    assert_eq!(dst.layout.borrow().focus(), &terminal(), "with the keys");
}

#[test]
fn a_base_terminal_whose_adopt_is_refused_starts_a_fresh_shell_on_its_next_show() {
    let dst = destination();
    dst.host.refuse_op(|op| matches!(op, HostOp::AdoptMoved { .. }));
    dst.committed_arrival();
    assert!(
        dst.ops().iter().any(is_adopt),
        "the premise: the adopt was asked and refused"
    );
    assert!(
        dst.layout.borrow().contains(&terminal()),
        "a base id stays in the layout"
    );
    assert!(!dst.layout.borrow().is_shown(&terminal()), "off screen");
    assert!(matches!(dst.state(&terminal()), Some(ModuleState::Killed(_))));
    assert!(
        !dst.ops()
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::Start { .. }))),
        "nothing started yet"
    );

    let mark = dst.mark();
    dst.run(Action::Module(terminal()));
    let ops = dst.ops_since(mark);
    assert!(
        ops.contains(&HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: terminal()
        })),
        "a fresh shell: {ops:#?}"
    );
    assert!(dst.layout.borrow().is_shown(&terminal()));
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
fn move_to_window_with_a_module_away_here_asks_the_other_window() {
    let src = source();
    crate::layout::place(&mut src.layout.borrow_mut(), &terminal2(), &editor(), Axis::Column).unwrap();
    src.layout.borrow_mut().set_away(&terminal2(), true).unwrap();
    let before = src.layout.borrow().clone();
    src.run(Action::MoveToWindow {
        module: Some(terminal2()),
        follow: true,
    });
    assert_eq!(
        src.sent().iter().map(|s| s.msg.clone()).collect::<Vec<_>>(),
        vec![Msg::MoveAsk {
            module: Some("terminal#2".into()),
            session: None,
            follow: true,
        }]
    );
    assert_eq!(
        *src.layout.borrow(),
        before,
        "nothing changes here until the other window offers it"
    );
    assert_eq!(src.toast(), None, "and nothing is refused");
}

#[test]
fn a_move_ask_offers_the_named_module() {
    let src = source();
    src.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: Some(terminal()),
        session: None,
        follow: true,
    });
    assert_eq!(src.kinds(), vec!["move_offer"]);
    let Msg::MoveOffer { module, kind, .. } = src.last_sent() else {
        panic!("an offer");
    };
    assert_eq!((module.as_str(), kind.as_str()), ("terminal", "terminal"));
    // It is this window's own move from here: the keys follow the module to the window that asked.
    src.ready(1);
    src.detached(1, serde_json::Value::Null, handoff());
    let mark = src.mark();
    src.done(1);
    assert!(src
        .ops_since(mark)
        .contains(&HostOp::RaisePeer { win: WIN, peer: PEER }));

    // A module this window does not hold, or one that may not go, is refused here; the asker learns nothing more.
    let src = source();
    src.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: Some(terminal2()),
        session: None,
        follow: false,
    });
    assert!(src.sent().is_empty());
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::NOT_HELD_TEXT));
    src.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: Some(editor()),
        session: None,
        follow: false,
    });
    assert!(src.sent().is_empty());
    assert_eq!(src.toast().as_deref(), Some(crate::wm::feedback::EDITOR_MOVE_TEXT));
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

#[test]
fn an_away_modules_key_sends_focus_to_the_other_window() {
    for origin in [Origin::Key { keycode: 7 }, Origin::Chip, Origin::Lua] {
        let end = source();
        crate::layout::place(&mut end.layout.borrow_mut(), &terminal2(), &editor(), Axis::Column).unwrap();
        end.layout.borrow_mut().set_away(&terminal2(), true).unwrap();
        let before = end.layout.borrow().clone();
        let mark = end.mark();
        if origin == Origin::Lua {
            crate::wm::fake::lua(&end.driver, WIN, crate::lua::layout::LayoutRequest::Focus(terminal2()));
        } else {
            end.driver.run(WIN, Action::Module(terminal2()), origin);
        }
        assert!(end
            .ops_since(mark)
            .contains(&HostOp::RaisePeer { win: WIN, peer: PEER }));
        assert_eq!(*end.layout.borrow(), before);
        assert_eq!(end.toast(), None);
        end.peer(PeerEvent::Raised {
            peer: PEER,
            token: Some("keys".into()),
        });
        assert_eq!(
            end.sent().last().unwrap().msg,
            Msg::Focus {
                target: FocusTarget::Module {
                    id: "terminal#2".into()
                },
                token: Some("keys".into())
            }
        );
        end.driver.detach_peer(WIN);
        end.driver.run(WIN, Action::Module(terminal2()), Origin::Chip);
        assert_eq!(end.toast().as_deref(), Some("'terminal#2' is in the other window"));
    }
    let end = source();
    end.peer(PeerEvent::Held {
        peer: PEER,
        ids: vec![terminal2()],
    });
    let mark = end.mark();
    end.run(Action::Module(terminal2()));
    assert!(end
        .ops_since(mark)
        .contains(&HostOp::RaisePeer { win: WIN, peer: PEER }));
    end.driver.detach_peer(WIN);
    end.run(Action::Module(terminal2()));
    assert_eq!(end.toast(), Some(crate::wm::feedback::not_in_window_text(&terminal2())));
}

#[test]
fn the_editor_key_in_window_2_raises_window_1s_editor() {
    let end = destination();
    end.run(Action::Module(editor()));
    end.peer(PeerEvent::Raised {
        peer: PEER,
        token: None,
    });
    assert_eq!(
        end.sent().last().unwrap().msg,
        Msg::Focus {
            target: FocusTarget::Module { id: "editor".into() },
            token: None
        }
    );
    end.driver.detach_peer(WIN);
    end.run(Action::Module(editor()));
    let asked = end.driver.query(|c| c.chrome(WIN).prompt).map(|prompt| prompt.text);
    assert_eq!(asked.as_deref(), Some(crate::wm::feedback::START_EDITOR_PROMPT));
}

#[test]
fn a_received_focus_presents_the_window_with_its_token_or_none() {
    for target in [FocusTarget::Window, FocusTarget::Module { id: "editor".into() }] {
        for token in [None, Some("activation".to_string())] {
            let end = source();
            let mark = end.mark();
            end.peer(PeerEvent::FocusIntent {
                peer: PEER,
                target: target.clone(),
                token: token.clone(),
            });
            assert!(end.ops_since(mark).contains(&HostOp::Present {
                win: WIN,
                activation: token.map(ActivationToken)
            }));
        }
    }
}

#[test]
fn arriving_keys_land_on_the_opposite_edges_module() {
    use crate::window_link::Side;
    let end = source();
    end.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Edge { side: Side::Right },
        token: None,
    });
    assert_eq!(end.layout.borrow().focus(), &editor());
    end.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Edge { side: Side::Left },
        token: None,
    });
    assert_eq!(end.layout.borrow().focus(), &terminal());
}

#[test]
fn the_window_says_its_monitor_on_change_and_at_attach() {
    let end = source();
    let monitor = Monitor {
        id: MonitorId("DP-1".into()),
        rect: crate::layout::Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        },
        scale: 1.0,
        name: "Left".into(),
    };
    end.driver.post(HostEvent::MonitorsChanged {
        monitors: vec![monitor.clone()],
    });
    end.driver.post(HostEvent::WindowMonitor {
        win: WIN,
        monitor: Some(monitor.id.clone()),
    });
    let expected = Msg::Monitor {
        connector: "DP-1".into(),
        description: "Left".into(),
    };
    assert_eq!(end.sent().last().unwrap().msg, expected);
    let count = end.sent().len();
    end.driver.post(HostEvent::WindowMonitor {
        win: WIN,
        monitor: Some(monitor.id.clone()),
    });
    end.driver.post(HostEvent::WindowMonitor {
        win: WIN,
        monitor: None,
    });
    assert_eq!(end.sent().len(), count);
    end.driver.detach_peer(WIN);
    end.driver.post(HostEvent::WindowMonitor {
        win: WIN,
        monitor: Some(monitor.id),
    });
    let port = FakePeerPort::new();
    end.driver.attach_peer(WIN, PEER, port.clone());
    assert!(port.sent().iter().any(|s| s.msg == expected));
}

#[test]
fn a_focus_intent_shows_a_hidden_editor_before_giving_it_the_keys() {
    let end = source();
    crate::layout::hide(
        &mut end.layout.borrow_mut(),
        &editor(),
        &crate::layout::Frame::new(GRID, 1),
    )
    .unwrap();
    end.peer(PeerEvent::FocusIntent {
        peer: PEER,
        target: FocusTarget::Module { id: "editor".into() },
        token: None,
    });
    assert!(end.layout.borrow().is_shown(&editor()));
    assert_eq!(end.layout.borrow().focus(), &editor());
}

#[test]
fn a_summary_is_sent_at_attach() {
    let e = source();
    let summaries: Vec<_> = e
        .port
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::Summary(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries,
        vec![Summary {
            workspaces: vec!["main".into(), "2".into()],
            ..Default::default()
        }]
    );
}

#[test]
fn a_burst_of_count_changes_sends_one_summary() {
    let e = source();
    for waiting in [1, 2, 3] {
        e.host.set_chat_summary(ChatSummary {
            waiting,
            running: 2,
            ..Default::default()
        });
        e.driver.post(HostEvent::Chat(ChatEvent::Attention {
            win: WIN,
            view: crate::tabs::ViewId::BASE,
        }));
        e.scheduler.advance(Duration::from_millis(50));
    }
    assert_eq!(
        e.port
            .sent()
            .iter()
            .filter(|s| matches!(s.msg, Msg::Summary(_)))
            .count(),
        1
    );
    e.scheduler.advance(Duration::from_millis(200));
    let summaries: Vec<_> = e
        .port
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::Summary(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[1].waiting, 3);
    assert_eq!(summaries[1].running, 2);
    e.host.set_chat_summary(ChatSummary {
        waiting: 3,
        running: 2,
        queued: 4,
        ..Default::default()
    });
    e.scheduler.advance(Duration::from_secs(1));
    assert_eq!(
        e.port
            .sent()
            .iter()
            .filter(|s| matches!(s.msg, Msg::Summary(_)))
            .count(),
        2
    );
    e.driver.post(HostEvent::Chat(ChatEvent::Busy {
        win: WIN,
        view: crate::tabs::ViewId::BASE,
        busy: true,
    }));
    e.scheduler.advance(Duration::from_millis(200));
    assert!(e
        .port
        .sent()
        .iter()
        .any(|s| matches!(&s.msg, Msg::Summary(s) if s.queued == 4)));
}

#[test]
fn a_summary_waiting_when_the_link_ends_is_never_sent() {
    let e = source();
    let before = e.port.sent().len();
    e.host.set_chat_summary(ChatSummary {
        waiting: 1,
        ..Default::default()
    });
    e.driver.post(HostEvent::Chat(ChatEvent::Attention {
        win: WIN,
        view: crate::tabs::ViewId::BASE,
    }));
    e.link_closed();
    e.scheduler.advance(Duration::from_secs(1));
    assert!(
        !e.port.sent()[before..].iter().any(|s| matches!(s.msg, Msg::Summary(_))),
        "the timer went with the link"
    );
}

#[test]
fn workspace_choose_lists_both_windows_workspaces_and_a_pick_in_the_other_sends_workspace() {
    let e = source();
    e.driver.post(HostEvent::Peer(PeerEvent::Summary {
        peer: PEER,
        summary: Summary {
            workspaces: vec!["<other>".into()],
            ..Default::default()
        },
    }));
    e.driver.run(WIN, Action::WorkspaceChoose, Origin::Lua);
    let model = e.driver.query(|c| c.chrome(WIN));
    assert!(format!("{:?}", model.prefix_strip).contains("3 2:<other>"));
    let spec = crate::keymap::KeySpec::parse("3").unwrap();
    e.driver.key(
        WIN,
        &KeyStage::Window,
        KeyPress {
            keycode: 10,
            press: crate::keymap::prefix::Press::Key(spec),
            key: Some(spec.key),
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        },
    );
    assert!(e
        .host
        .log()
        .iter()
        .any(|op| matches!(op, Logged::Op(HostOp::RaisePeer { .. }, _))));
    e.driver.post(HostEvent::Peer(PeerEvent::Raised {
        peer: PEER,
        token: Some("activation".into()),
    }));
    assert!(e.port.sent().iter().any(|s| matches!(&s.msg, Msg::Workspace { name, token } if name == "<other>" && token.as_deref() == Some("activation"))));
}

#[test]
fn a_workspace_message_switches_this_window() {
    let e = source();
    e.driver.post(HostEvent::Peer(PeerEvent::Workspace {
        peer: PEER,
        name: "2".into(),
        token: Some("token".into()),
    }));
    assert_eq!(e.workspaces.borrow().current(), "2");
    assert!(e.host.log().iter().any(
        |op| matches!(op, Logged::Op(HostOp::Present { activation: Some(ActivationToken(t)), .. }, _) if t == "token")
    ));
    e.driver.post(HostEvent::Peer(PeerEvent::Workspace {
        peer: PEER,
        name: "unknown".into(),
        token: None,
    }));
    assert_eq!(e.workspaces.borrow().order().len(), 2);
    assert!(e.driver.query(|c| c.chrome(WIN)).toast.is_some());
}

#[test]
fn a_tab_verb_summarizes_counts_after_the_host_applies_it() {
    let e = source();
    e.host.on_apply(|host, _, op| {
        if matches!(op, HostOp::Chat(ChatOp::Tab { .. })) {
            host.set_chat_summary(ChatSummary {
                queued: 5,
                ..Default::default()
            });
        }
    });
    e.driver.post(HostEvent::Chat(ChatEvent::TabVerb {
        win: WIN,
        view: crate::tabs::ViewId::BASE,
        action: Action::Tab(crate::keymap::TabAction::New),
    }));
    e.scheduler.advance(Duration::from_millis(200));
    assert!(e
        .port
        .sent()
        .iter()
        .any(|s| matches!(&s.msg, Msg::Summary(s) if s.queued == 5)));
}

// A chat.

fn agent2() -> ModuleId {
    ModuleId::parse("agent#2").unwrap()
}
fn view2() -> ViewId {
    ViewId::of_module(&agent2()).unwrap()
}
const TAB: crate::tabs::TabId = crate::tabs::TabId(7);
const SESSION: &str = "sess-moving";

fn facts(live: bool, running: bool) -> ChatMoveFacts {
    ChatMoveFacts {
        tab: TAB,
        session: Some(SESSION.into()),
        live,
        running,
    }
}

/// Window 1 (`role`, `presence`): `editor | (agent / agent#2)`, the keys on `agent#2`, whose tab may move.
fn chat_window(role: WindowRole, presence: EditorPresence) -> End {
    let layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(agent2())),
        ),
        agent2(),
    )
    .unwrap();
    let end = open(layout, role, presence);
    end.host.set_chat_move(view2(), Ok(facts(true, false)));
    end.host.set_chat_move(ViewId::BASE, Ok(facts(true, false)));
    end.host.set_module_size(agent2(), Size { w: 500, h: 300 });
    end
}

fn chat_source() -> End {
    chat_window(WindowRole::Main, EditorPresence::Owned)
}

fn freeze(view: ViewId, id: u64) -> HostOp {
    HostOp::Chat(ChatOp::Freeze {
        win: WIN,
        view,
        tab: Some(TAB),
        mv: MoveId(id),
    })
}

fn chat_forget(view: ViewId) -> HostOp {
    HostOp::Chat(ChatOp::Forget {
        win: WIN,
        view,
        tab: Some(TAB),
    })
}

fn chat_resume(view: ViewId) -> HostOp {
    HostOp::Chat(ChatOp::Resume {
        win: WIN,
        view,
        tab: Some(TAB),
    })
}

fn review_hold(hold: bool) -> HostOp {
    HostOp::Chat(ChatOp::ReviewHold {
        win: WIN,
        session: SESSION.into(),
        hold,
        here: true,
    })
}

/// The held events released, to this window (`here`) or the other.
fn review_release(here: bool) -> HostOp {
    HostOp::Chat(ChatOp::ReviewHold {
        win: WIN,
        session: SESSION.into(),
        hold: false,
        here,
    })
}

fn hold_presence(id: u64) -> HostOp {
    HostOp::Chat(ChatOp::HoldPresence {
        win: WIN,
        session: SESSION.into(),
        mv: MoveId(id),
    })
}

impl End {
    fn move_chat(&self, module: ModuleId) {
        self.run(Action::MoveToWindow {
            module: Some(module),
            follow: false,
        });
    }

    fn chat(&self, event: ChatEvent) {
        self.driver.post(HostEvent::Chat(event));
    }

    fn chat_frozen(&self, view: ViewId, id: u64) {
        self.chat(ChatEvent::Frozen {
            win: WIN,
            view,
            mv: MoveId(id),
            record: serde_json::json!({ "tab": "record" }),
            handoff: handoff(),
        });
    }

    fn offer_chat(&self, id: u64, module: ModuleId) {
        self.peer(PeerEvent::MoveOffer {
            peer: PEER,
            id: MoveId(id),
            module,
            kind: MoveKind::Chat,
            shape: Shape::Tiled,
            size: (500, 300),
            session: Some(SESSION.into()),
        });
    }

    fn count(&self, op: &HostOp) -> usize {
        self.ops().iter().filter(|o| *o == op).count()
    }
}

#[test]
fn an_idle_chat_view_moves_and_is_away_in_the_source() {
    let src = chat_source();
    src.move_chat(agent2());
    assert_eq!(
        src.last_sent(),
        Msg::MoveOffer {
            mv: 1,
            module: "agent#2".into(),
            kind: "chat".into(),
            shape: Shape::Tiled,
            size: (500, 300),
            session: Some(SESSION.into()),
        }
    );
    assert!(
        !src.ops()
            .iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::Freeze { .. }))),
        "nothing frozen before move_ready"
    );
    src.ready(1);
    assert_eq!(
        src.count(&freeze(view2(), 1)),
        1,
        "frozen, its tab named, once the other window is ready"
    );
    src.chat_frozen(view2(), 1);
    assert_eq!(src.kinds(), vec!["move_offer", "move"]);
    assert!(!src.away(&agent2()), "here until move_done");

    let mark = src.mark();
    src.done(1);
    let ops = src.ops_since(mark);
    assert!(ops.contains(&chat_forget(view2())), "the tab is let go: {ops:#?}");
    assert!(
        !ops.iter().any(|op| matches!(op, HostOp::Terminal(_))),
        "no terminal op for a chat: {ops:#?}"
    );
    assert!(src.away(&agent2()), "the view is away, its leaf kept");
    assert_eq!(src.state(&agent2()), Some(ModuleState::Away));
    assert!(src.workspaces.borrow().stored("2").unwrap().is_away(&agent2()));
    assert_ne!(
        src.layout.borrow().focus(),
        &agent2(),
        "the keys went to another module"
    );

    // The other window takes it as a view of its own, laid out off screen first and declared a chat view.
    let dst = destination();
    dst.offer_chat(1, agent2());
    assert!(dst.ops().contains(&HostOp::PrepareSurface {
        win: WIN,
        module: agent2(),
        shape: Shape::Tiled,
        size: (500, 300),
    }));
    assert_eq!(dst.kinds(), vec!["move_ready"]);
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    dst.chat(ChatEvent::PresenceHeld {
        win: WIN,
        mv: MoveId(1),
    });
    dst.port.answer(0, Ok(()));
    dst.woke();
    let adopt = dst.ops().into_iter().find(is_adopt).expect("adopted");
    assert!(
        matches!(&adopt, HostOp::AdoptMoved { module, kind: MoveKind::Chat, .. } if *module == agent2()),
        "{adopt:#?}"
    );
    assert!(dst.layout.borrow().is_shown(&agent2()));
    assert_eq!(
        dst.driver
            .query(|c| c.lifecycle(WIN).and_then(|l| l.kind(&agent2()).cloned())),
        Some(SurfaceKind::Chat { view: view2() })
    );
}

#[test]
fn a_base_view_tab_moves_and_nothing_is_marked_away() {
    let src = chat_source();
    src.move_chat(agent());
    let Msg::MoveOffer { module, kind, .. } = src.last_sent() else {
        panic!("an offer");
    };
    assert_eq!((module.as_str(), kind.as_str()), ("agent", "chat"));
    src.ready(1);
    assert_eq!(src.count(&freeze(ViewId::BASE, 1)), 1);
    src.chat_frozen(ViewId::BASE, 1);
    let focus = src.layout.borrow().focus().clone();
    let mark = src.mark();
    src.done(1);
    let ops = src.ops_since(mark);
    assert!(ops.contains(&chat_forget(ViewId::BASE)), "{ops:#?}");
    assert!(!src.away(&agent()), "the base view stays here");
    assert!(src.layout.borrow().is_shown(&agent()));
    assert_eq!(src.layout.borrow().focus(), &focus, "the keys stay where they were");
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Unmap { module, .. } if *module == agent())),
        "{ops:#?}"
    );

    // The other window's base view takes the tab: no place is made, move_ready goes at once.
    let dst = destination();
    let before = dst.layout.borrow().clone();
    dst.offer_chat(1, agent());
    assert!(
        !dst.ops().iter().any(|op| matches!(op, HostOp::PrepareSurface { .. })),
        "nothing to prepare"
    );
    assert_eq!(dst.kinds(), vec!["move_ready"]);
    dst.deliver(1, Descriptors::new(vec![pipe_fd(), pipe_fd()]));
    dst.chat(ChatEvent::PresenceHeld {
        win: WIN,
        mv: MoveId(1),
    });
    dst.port.answer(0, Ok(()));
    dst.woke();
    let ops = dst.ops();
    let adopt = ops.iter().find(|op| is_adopt(op)).expect("adopted");
    let HostOp::AdoptMoved { module, kind, fds, .. } = adopt else {
        unreachable!()
    };
    assert_eq!((module, kind), (&agent(), &MoveKind::Chat));
    assert_eq!(fds.len(), 2, "the descriptors go with the adopt");
    assert_eq!(
        *dst.layout.borrow(),
        before,
        "nothing placed: the base view is already here"
    );
    assert!(!ops.iter().any(|op| matches!(op, HostOp::DropSurface { .. })));
}

#[test]
fn a_chat_freeze_refusal_gives_up_only_its_own_move() {
    let src = chat_source();
    src.move_chat(agent2());
    src.ready(1);
    // Move 1's freeze takes too long: it is given up and the tab resumes.
    src.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(src.count(&chat_resume(view2())), 1);
    src.move_chat(agent2());
    src.ready(3);
    assert_eq!(src.count(&freeze(view2(), 3)), 1, "move 3 freezes the tab again");
    let sent = src.kinds().len();

    // Move 1's late refusal names move 1: move 3 goes on.
    src.chat(ChatEvent::FreezeRefused {
        win: WIN,
        view: view2(),
        mv: MoveId(1),
        why: "the conversation is folding its events".into(),
    });
    assert_eq!(src.kinds().len(), sent, "nothing sent for an ended move's refusal");
    assert_eq!(src.count(&chat_resume(view2())), 1, "and nothing resumed");
    src.chat_frozen(view2(), 3);
    assert_eq!(src.kinds().last(), Some(&"move"), "move 3 is sent");

    // A refusal of the move that waits for it gives that move up.
    let src = chat_source();
    src.move_chat(agent2());
    src.ready(1);
    src.chat(ChatEvent::FreezeRefused {
        win: WIN,
        view: view2(),
        mv: MoveId(1),
        why: "a message sent into the turn has not been read yet".into(),
    });
    assert_eq!(
        src.last_sent(),
        Msg::MoveAbort {
            mv: 1,
            why: "a message sent into the turn has not been read yet".into()
        }
    );
    assert_eq!(src.count(&chat_resume(view2())), 1);
    assert_eq!(
        src.toast().as_deref(),
        Some("a message sent into the turn has not been read yet")
    );

    // A host that refuses the freeze itself gives it up at once, by the view the refused op names.
    let src = chat_source();
    src.host
        .refuse_op(|op| matches!(op, HostOp::Chat(ChatOp::Freeze { .. })));
    src.move_chat(agent2());
    src.ready(1);
    assert!(matches!(src.last_sent(), Msg::MoveAbort { mv: 1, .. }));
}

#[test]
fn the_destinations_running_file_is_held_before_move_done_is_sent() {
    let dst = destination();
    dst.offer_chat(1, agent2());
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    assert_eq!(
        dst.count(&hold_presence(1)),
        1,
        "the presence is asked for the arriving session"
    );
    assert_eq!(dst.kinds(), vec!["move_ready"], "move_done waits for the presence");
    dst.woke();
    assert!(!dst.ops().iter().any(is_adopt));

    dst.chat(ChatEvent::PresenceHeld {
        win: WIN,
        mv: MoveId(1),
    });
    assert_eq!(dst.kinds(), vec!["move_ready", "move_done"]);
    assert!(dst.sent()[1].confirmed);
    assert!(!dst.ops().iter().any(is_adopt), "built only once move_done is written");
    dst.port.answer(0, Ok(()));
    dst.woke();
    assert_eq!(dst.ops().iter().filter(|op| is_adopt(op)).count(), 1);

    // A presence that cannot hold the turn gives the arrival up: the source keeps the chat.
    let dst = destination();
    dst.offer_chat(1, agent2());
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    dst.chat(ChatEvent::PresenceRefused {
        win: WIN,
        mv: MoveId(1),
        why: "the presence thread is not running".into(),
    });
    assert_eq!(
        dst.last_sent(),
        Msg::MoveAbort {
            mv: 1,
            why: "the presence thread is not running".into()
        }
    );
    let ops = dst.ops();
    assert!(ops.contains(&HostOp::Chat(ChatOp::DropPresence {
        win: WIN,
        session: SESSION.into(),
        mv: MoveId(1),
    })));
    assert!(ops.contains(&HostOp::DropSurface {
        win: WIN,
        module: agent2(),
        why: DropWhy::MoveGivenUp,
    }));
    dst.chat(ChatEvent::PresenceHeld {
        win: WIN,
        mv: MoveId(1),
    });
    assert!(!dst.kinds().contains(&"move_done"), "a late answer sends nothing");

    // Nor does a presence that never answers hold the move past its deadline.
    let dst = destination();
    dst.offer_chat(1, agent2());
    dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
    dst.scheduler.advance(MOVE_DEADLINE);
    assert_eq!(
        dst.last_sent(),
        Msg::MoveAbort {
            mv: 1,
            why: crate::wm::feedback::PRESENCE_TOO_SLOW_TEXT.into()
        }
    );
    assert!(!dst.kinds().contains(&"move_done"));
}

#[test]
fn window_1_holds_the_sessions_review_from_the_offer_to_the_end() {
    // As the source: held from the offer, released at the commit.
    let src = chat_source();
    src.move_chat(agent2());
    assert_eq!(src.count(&review_hold(true)), 1, "held from the offer");
    src.ready(1);
    src.chat_frozen(view2(), 1);
    assert_eq!(src.count(&review_release(false)), 0, "held while the move is under way");
    src.done(1);
    assert_eq!(
        src.count(&review_release(false)),
        1,
        "released at the commit, to the other window, which holds the session now"
    );

    // A move given up releases them too, as does the link's end.
    let src = chat_source();
    src.move_chat(agent2());
    src.peer(PeerEvent::MoveRefused {
        peer: PEER,
        id: MoveId(1),
        why: "no".into(),
    });
    assert_eq!(src.count(&review_release(true)), 1);
    let src = chat_source();
    src.move_chat(agent2());
    src.ready(1);
    src.chat_frozen(view2(), 1);
    src.link_closed();
    assert_eq!(src.count(&review_release(true)), 1);
    assert_eq!(src.count(&chat_resume(view2())), 1);

    // As the destination: held from the offer, released once the chat is built here or given up.
    let layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
        editor(),
    )
    .unwrap();
    let dst = open(layout.clone(), WindowRole::Main, EditorPresence::Owned);
    dst.offer_chat(2, agent2());
    assert_eq!(dst.count(&review_hold(true)), 1);
    dst.deliver(2, Descriptors::new(vec![pipe_fd()]));
    dst.chat(ChatEvent::PresenceHeld {
        win: WIN,
        mv: MoveId(2),
    });
    assert_eq!(dst.count(&review_release(true)), 0);
    dst.port.answer(0, Ok(()));
    dst.woke();
    assert_eq!(dst.count(&review_release(true)), 1);
    let dst = open(layout, WindowRole::Main, EditorPresence::Owned);
    dst.offer_chat(2, agent2());
    dst.link_closed();
    assert_eq!(
        dst.count(&review_release(false)),
        1,
        "an arrival the link's end drops releases them"
    );

    // A window that does not run nvim holds nothing.
    let dst = destination();
    dst.offer_chat(1, agent2());
    let src = chat_window(WindowRole::Linked { side: LinkSide::Second }, EditorPresence::Proxy);
    src.move_chat(agent2());
    for end in [&dst, &src] {
        assert!(!end
            .ops()
            .iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::ReviewHold { .. }))));
    }
}

#[test]
fn a_move_is_refused_while_a_steer_is_open() {
    let src = chat_source();
    src.host.set_chat_move(
        view2(),
        Err("a message sent into the turn has not been read yet".into()),
    );
    src.move_chat(agent2());
    assert_eq!(
        src.toast().as_deref(),
        Some("a message sent into the turn has not been read yet")
    );
    assert!(src.kinds().is_empty(), "nothing offered: {:?}", src.kinds());
    assert!(!src
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Chat(ChatOp::ReviewHold { .. } | ChatOp::Freeze { .. }))));
}

#[test]
fn with_live_moves_off_a_running_chat_moves_at_its_turns_end_with_the_note() {
    let src = chat_source();
    src.host.set_chat_move(view2(), Ok(facts(false, true)));
    src.move_chat(agent2());
    assert_eq!(src.toast().as_deref(), Some("moves to window 2 when this turn ends"));
    assert!(src.kinds().is_empty(), "not offered while the turn runs");
    // Another view's turn ending changes nothing.
    src.chat(ChatEvent::Busy {
        win: WIN,
        view: ViewId::BASE,
        busy: false,
    });
    assert!(src.kinds().is_empty());
    src.host.set_chat_move(view2(), Ok(facts(false, false)));
    src.chat(ChatEvent::Busy {
        win: WIN,
        view: view2(),
        busy: false,
    });
    assert_eq!(src.kinds(), vec!["move_offer"], "offered once its turn ends");
    src.chat(ChatEvent::Busy {
        win: WIN,
        view: view2(),
        busy: false,
    });
    assert_eq!(src.kinds(), vec!["move_offer"], "offered once");

    // A live agent's running chat moves at once; the queue goes with the link.
    let src = chat_source();
    src.host.set_chat_move(view2(), Ok(facts(true, true)));
    src.move_chat(agent2());
    assert_eq!(src.kinds(), vec!["move_offer"]);
    let src = chat_source();
    src.host.set_chat_move(view2(), Ok(facts(false, true)));
    src.move_chat(agent2());
    src.link_closed();
    src.host.set_chat_move(view2(), Ok(facts(false, false)));
    src.chat(ChatEvent::Busy {
        win: WIN,
        view: view2(),
        busy: false,
    });
    assert!(src.kinds().is_empty(), "no move waits past the link's end");
}

#[test]
fn enter_in_the_chooser_on_a_session_the_other_window_holds_moves_it_here() {
    // The window whose chooser was used asks for the session, the keys following it.
    let here = chat_source();
    here.chat(ChatEvent::ChooserPartnerPick {
        win: WIN,
        view: ViewId::BASE,
        session: SESSION.into(),
    });
    assert_eq!(
        here.last_sent(),
        Msg::MoveAsk {
            module: None,
            session: Some(SESSION.into()),
            follow: true,
        }
    );
    assert!(
        !here.ops().iter().any(|op| matches!(op, HostOp::RaisePeer { .. })),
        "the other window is not raised: the chat comes here"
    );

    // The window holding it finds the view by the session and offers it.
    let there = chat_window(WindowRole::Linked { side: LinkSide::Second }, EditorPresence::Proxy);
    there.host.set_view_of_session(SESSION, view2());
    there.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: None,
        session: Some(SESSION.into()),
        follow: true,
    });
    let Msg::MoveOffer {
        module, kind, session, ..
    } = there.last_sent()
    else {
        panic!("an offer: {:?}", there.kinds());
    };
    assert_eq!(
        (module.as_str(), kind.as_str(), session.as_deref()),
        ("agent#2", "chat", Some(SESSION))
    );
    there.ready(2);
    there.chat_frozen(view2(), 2);
    there.done(2);
    assert!(
        there.ops().iter().any(|op| matches!(op, HostOp::RaisePeer { .. })),
        "the keys follow it to the window that asked"
    );

    // A session neither window's chats hold offers nothing.
    let there = chat_window(WindowRole::Linked { side: LinkSide::Second }, EditorPresence::Proxy);
    there.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: None,
        session: Some("nobody's".into()),
        follow: true,
    });
    assert!(there.kinds().is_empty());
}

#[test]
fn a_chat_view_whose_tab_went_to_the_other_window_is_not_killed_as_empty() {
    let src = chat_source();
    src.move_chat(agent2());
    src.ready(1);
    src.chat_frozen(view2(), 1);
    src.done(1);
    let mark = src.mark();
    // The panel says the view holds no tab once its tab has left.
    src.chat(ChatEvent::ViewEmpty {
        win: WIN,
        view: view2(),
    });
    src.scheduler.advance(Duration::from_millis(1));
    assert!(
        src.layout.borrow().contains(&agent2()) && src.away(&agent2()),
        "the placeholder stays"
    );
    assert_eq!(src.state(&agent2()), Some(ModuleState::Away));
    assert!(
        !src.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::DropSurface { .. } | HostOp::Chat(ChatOp::CloseTabs { .. }))),
        "{:#?}",
        src.ops_since(mark)
    );
}

fn page_id() -> ModuleId {
    ModuleId::lua("markdown")
}

fn page_end(plugin: bool, hidden: bool, second: bool) -> End {
    let id = page_id();
    let mut layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(id.clone())),
        agent(),
    )
    .unwrap();
    if hidden {
        layout = Layout::from_parts(
            Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(id.clone())),
            BTreeSet::from([id.clone()]),
            agent(),
        )
        .unwrap();
    }
    if second {
        layout.set_away(&id, true).unwrap();
    }
    let surface = SurfaceDecl {
        module: id,
        kind: if plugin {
            SurfaceKind::PluginPage {
                plugin: "markdown".into(),
            }
        } else {
            SurfaceKind::LuaPanel {
                url: "eitri://markdown".into(),
                title: "Markdown".into(),
            }
        },
        deferred: hidden,
    };
    open_with(
        layout,
        if second {
            WindowRole::Linked { side: LinkSide::Second }
        } else {
            WindowRole::Main
        },
        if second {
            EditorPresence::Proxy
        } else {
            EditorPresence::Owned
        },
        Some(surface),
    )
}

fn page_move_id(dst: &End) -> MoveId {
    if dst
        .driver
        .query(|c| c.window_spec(WIN).is_some_and(|s| s.role == WindowRole::Main))
    {
        MoveId(2)
    } else {
        MoveId(1)
    }
}

fn offer_page(dst: &End, plugin: bool, built: bool) {
    dst.peer(PeerEvent::MoveOffer {
        peer: PEER,
        id: page_move_id(dst),
        module: page_id(),
        kind: if plugin {
            MoveKind::PluginPage
        } else {
            MoveKind::LuaPanel
        },
        shape: Shape::Tiled,
        size: if built { (600, 400) } else { (0, 0) },
        session: None,
    });
}

#[test]
fn an_offer_of_a_page_this_window_holds_itself_is_refused_and_touches_nothing() {
    for hidden in [false, true] {
        // The page is window 1's own here, shown or hidden: never away.
        let dst = page_end(true, hidden, false);
        let mark = dst.mark();
        // Window 2's offers carry even numbers.
        dst.peer(PeerEvent::MoveOffer {
            peer: PEER,
            id: MoveId(2),
            module: page_id(),
            kind: MoveKind::PluginPage,
            shape: Shape::Tiled,
            size: (600, 400),
            session: None,
        });
        assert_eq!(
            dst.last_sent(),
            Msg::MoveRefused {
                mv: 2,
                why: crate::wm::feedback::HELD_HERE_TEXT.into()
            }
        );
        let ops = dst.ops_since(mark);
        assert!(
            !ops.iter().any(|op| matches!(op, HostOp::PrepareSurface { .. })),
            "the page shown here is not made ready for another: {ops:#?}"
        );
        // An abort for that move afterwards drops nothing here.
        dst.peer(PeerEvent::MoveAbort {
            peer: PEER,
            id: MoveId(2),
            why: "gave up".into(),
        });
        assert!(!dst.away(&page_id()));
        assert!(!dst
            .ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::Unmap { .. } | HostOp::DropSurface { .. })));
    }
}

fn commit_page(dst: &End, built: bool) {
    offer_page(dst, true, built);
    if built {
        dst.driver.post(HostEvent::Prepared {
            win: WIN,
            module: page_id(),
        });
    }
    dst.peer(PeerEvent::Move {
        peer: PEER,
        id: page_move_id(dst),
        record: serde_json::json!({"built": built, "state": {"scroll": 12}}),
        fds: Descriptors::default(),
    });
    dst.port.answer(0, Ok(()));
    dst.woke();
}

fn mapped_page(e: &End) -> usize {
    e.ops()
        .iter()
        .filter(|op| {
            **op == HostOp::Map {
                win: WIN,
                module: page_id(),
            }
        })
        .count()
}

#[test]
fn a_lua_panel_moved_is_shown_from_the_destinations_producer_and_away_here() {
    let src = page_end(false, false, false);
    src.move_chat(page_id());
    src.ready(1);
    assert!(matches!(
        src.last_sent(),
        Msg::Move {
            record: serde_json::Value::Null,
            ..
        }
    ));
    assert!(!src.ops().iter().any(|op| matches!(
        op,
        HostOp::Plugin(PluginOp::Freeze { .. }) | HostOp::Terminal(TermOp::Detach { .. })
    )));
    src.done(1);
    assert!(src.away(&page_id()));
    let dst = page_end(false, false, true);
    offer_page(&dst, false, false);
    assert_eq!(dst.kinds(), vec!["move_ready"]);
    dst.peer(PeerEvent::Move {
        peer: PEER,
        id: MoveId(1),
        record: serde_json::Value::Null,
        fds: Descriptors::default(),
    });
    dst.port.answer(0, Ok(()));
    dst.woke();
    assert!(!dst.away(&page_id()));
    assert_eq!(
        mapped_page(&dst),
        1,
        "the panel taken back is mapped by the change that shows it: {:#?}",
        dst.ops()
    );
    assert!(!dst.ops().iter().any(is_adopt));
    assert!(dst.ops().contains(&HostOp::Plugin(PluginOp::Reload {
        win: WIN,
        module: page_id(),
        reset_guard: true
    })));
}

#[test]
fn a_page_freeze_refusal_gives_up_only_its_own_move() {
    let src = page_end(true, false, false);
    src.host.set_page_built(page_id(), true);
    src.move_chat(page_id());
    src.ready(1);
    assert!(src.ops().contains(&HostOp::Plugin(PluginOp::Freeze {
        win: WIN,
        module: page_id(),
        mv: MoveId(1)
    })));
    src.driver.post(HostEvent::Plugin(PluginEvent::FreezeRefused {
        win: WIN,
        module: page_id(),
        mv: MoveId(1),
        why: "busy".into(),
    }));
    src.move_chat(page_id());
    src.ready(3);
    src.driver.post(HostEvent::Plugin(PluginEvent::FreezeRefused {
        win: WIN,
        module: page_id(),
        mv: MoveId(1),
        why: "late".into(),
    }));
    src.driver.post(HostEvent::Plugin(PluginEvent::Frozen {
        win: WIN,
        module: page_id(),
        mv: MoveId(3),
        record: serde_json::Value::Null,
        handoff: Handoff::new(Rc::new(())),
    }));
    assert!(matches!(src.last_sent(), Msg::Move { mv: 3, .. }));
}

fn page_ready_event(dst: &End) {
    dst.driver.post(HostEvent::Plugin(PluginEvent::Ready {
        win: WIN,
        module: page_id(),
    }));
}

type PageRectangles = Vec<(ModuleId, crate::layout::Rect)>;

fn page_rectangles(dst: &End) -> PageRectangles {
    dst.driver.allocate(WIN, GRID).modules.clone()
}

fn watch_page_allocations(dst: &End) -> Rc<RefCell<Vec<(bool, PageRectangles)>>> {
    let allocations = Rc::new(RefCell::new(vec![]));
    let seen = allocations.clone();
    let driver = Rc::downgrade(&dst.driver);
    dst.host.on_apply(move |host, _, op| {
        if matches!(op, HostOp::Allocate { .. }) {
            let mapped = host
                .log()
                .iter()
                .any(|e| matches!(e, Logged::Op(HostOp::Map { module, .. }, _) if *module == page_id()));
            seen.borrow_mut()
                .push((mapped, driver.upgrade().unwrap().allocate(WIN, GRID).modules.clone()));
        }
    });
    allocations
}

#[test]
fn a_built_page_arrival_changes_the_arrangement_only_when_it_is_ready() {
    let dst = page_end(true, false, true);
    let before = page_rectangles(&dst);
    let allocations = watch_page_allocations(&dst);
    commit_page(&dst, true);
    assert!(dst.layout.borrow().contains(&page_id()));
    assert!(!dst.away(&page_id()));
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    assert_eq!(page_rectangles(&dst), before);
    assert!(allocations
        .borrow()
        .iter()
        .all(|(mapped, rects)| !mapped && *rects == before));
    assert_eq!(dst.layout.borrow().focus(), &agent());
    let stored_shown = dst.workspaces.borrow().stored("2").unwrap().is_shown(&page_id());
    allocations.borrow_mut().clear();
    page_ready_event(&dst);
    assert_eq!(
        dst.workspaces.borrow().stored("2").unwrap().is_shown(&page_id()),
        stored_shown
    );
    assert_eq!(mapped_page(&dst), 1);
    assert!(dst.layout.borrow().is_shown(&page_id()));
    let allocations = allocations.borrow();
    assert_eq!(allocations.len(), 1);
    assert!(
        allocations[0].0,
        "the page is mapped in the pass that allocates its rectangle"
    );
    assert!(allocations[0].1.iter().any(|(id, _)| *id == page_id()));
    assert_eq!(dst.layout.borrow().focus(), &agent());
}

#[test]
fn a_waiting_page_is_shown_only_in_its_arrival_workspace_after_a_switch() {
    for timeout in [false, true] {
        let dst = page_end(true, true, true);
        commit_page(&dst, true);
        dst.run(Action::WorkspaceSwitch { name: "2".into() });
        assert_eq!(dst.workspaces.borrow().current(), "2");
        assert!(!dst.layout.borrow().is_shown(&page_id()));
        let before = page_rectangles(&dst);
        let stored_before = dst.layout.borrow().clone();

        if timeout {
            dst.scheduler.advance(Duration::from_secs(1));
        } else {
            page_ready_event(&dst);
        }

        assert!(!dst.layout.borrow().is_shown(&page_id()));
        assert_eq!(page_rectangles(&dst), before);
        assert_eq!(mapped_page(&dst), 0);
        assert!(dst.workspaces.borrow().stored("main").unwrap().is_shown(&page_id()));

        dst.run(Action::WorkspaceSwitch { name: "main".into() });
        assert!(dst.layout.borrow().is_shown(&page_id()));
        assert_eq!(mapped_page(&dst), 1);
        assert_eq!(dst.workspaces.borrow().stored("2").unwrap(), &stored_before);
        page_ready_event(&dst);
        dst.scheduler.advance(Duration::from_secs(2));
        assert_eq!(mapped_page(&dst), 1);
    }
}

#[test]
fn a_waiting_page_follows_its_workspace_rename() {
    for switched in [false, true] {
        for timeout in [false, true] {
            let dst = page_end(true, true, true);
            dst.run(Action::WorkspaceSwitch { name: "2".into() });
            commit_page(&dst, true);
            dst.run(Action::WorkspaceRename);
            dst.driver.post(HostEvent::TopBarEntry {
                win: WIN,
                ev: EntryEv::Enter("study".into()),
            });
            assert_eq!(dst.workspaces.borrow().current(), "study");
            if switched {
                dst.run(Action::WorkspaceSwitch { name: "main".into() });
            }
            if timeout {
                dst.scheduler.advance(Duration::from_secs(1));
            } else {
                page_ready_event(&dst);
            }
            if switched {
                assert!(!dst.layout.borrow().is_shown(&page_id()));
                assert!(dst.workspaces.borrow().stored("study").unwrap().is_shown(&page_id()));
                dst.run(Action::WorkspaceSwitch { name: "study".into() });
            }
            assert!(dst.layout.borrow().is_shown(&page_id()));
            assert_eq!(mapped_page(&dst), 1);
            page_ready_event(&dst);
            dst.scheduler.advance(Duration::from_secs(2));
            assert_eq!(mapped_page(&dst), 1);
        }
    }
}

#[test]
fn a_waiting_page_uses_the_live_layout_if_its_workspace_is_gone() {
    for timeout in [false, true] {
        let dst = page_end(true, true, true);
        dst.run(Action::WorkspaceSwitch { name: "2".into() });
        commit_page(&dst, true);
        dst.run(Action::WorkspaceSwitch { name: "main".into() });
        dst.workspaces.borrow_mut().delete("2").unwrap();
        if timeout {
            dst.scheduler.advance(Duration::from_secs(1));
        } else {
            page_ready_event(&dst);
        }
        assert!(dst.layout.borrow().is_shown(&page_id()));
        assert_eq!(mapped_page(&dst), 1);
    }
}

#[test]
fn the_link_ending_while_a_page_waits_shows_and_maps_it_once() {
    let other = page_end(true, false, true);
    let layout = other.layout.borrow().clone();
    let dst = open_with(
        layout,
        WindowRole::Main,
        EditorPresence::Owned,
        Some(SurfaceDecl {
            module: page_id(),
            kind: SurfaceKind::PluginPage {
                plugin: "markdown".into(),
            },
            deferred: false,
        }),
    );
    commit_page(&dst, true);
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    let allocations = watch_page_allocations(&dst);
    let mark = dst.mark();
    dst.driver.post(HostEvent::Pair(PairEvent::LinkEnded {
        win: WIN,
        who: "window 2".into(),
    }));
    let ended = dst
        .notes()
        .into_iter()
        .find(|line| line.contains("ended: back"))
        .unwrap();
    assert!(!ended.contains(page_id().as_str()), "{ended}");
    assert!(!dst
        .ops_since(mark)
        .iter()
        .any(|op| { matches!(op, HostOp::Pair(PairOp::ReloadPage { module, .. }) if *module == page_id()) }));
    assert_eq!(mapped_page(&dst), 0);
    allocations.borrow_mut().clear();
    dst.driver.detach_peer(WIN);
    assert!(dst.layout.borrow().is_shown(&page_id()));
    assert_eq!(mapped_page(&dst), 1);
    let allocations = allocations.borrow();
    assert_eq!(allocations.len(), 1);
    assert!(allocations[0].0);
    assert!(allocations[0].1.iter().any(|(id, _)| *id == page_id()));
    page_ready_event(&dst);
    dst.scheduler.advance(Duration::from_secs(2));
    dst.driver.detach_peer(WIN);
    assert_eq!(mapped_page(&dst), 1);
}

#[test]
fn a_ready_page_says_how_long_its_arrival_waited_and_a_late_one_that_none_waited() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    dst.scheduler.advance(Duration::from_millis(300));
    page_ready_event(&dst);
    page_ready_event(&dst);
    let said: Vec<String> = dst
        .notes()
        .into_iter()
        .filter(|line| line.contains("page ready after") || line.contains("page.ready"))
        .collect();
    assert_eq!(
        said,
        [
            format!("[move] {}: page ready after 300 ms", page_id()),
            format!("[move] {}: page.ready with no arrival waiting", page_id()),
        ]
    );

    let timed_out = page_end(true, false, true);
    commit_page(&timed_out, true);
    timed_out.scheduler.advance(Duration::from_secs(2));
    assert!(!timed_out.notes().iter().any(|line| line.contains("page ready after")));
}

#[test]
fn a_waiting_page_is_told_it_is_on_screen_before_it_is_mapped() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    let on_screen = HostOp::Plugin(PluginOp::OnScreen {
        win: WIN,
        module: page_id(),
        on: true,
    });
    assert!(dst.ops().contains(&on_screen), "{:?}", dst.ops());
    assert_eq!(mapped_page(&dst), 0, "still covered");
    let mark = dst.mark();
    page_ready_event(&dst);
    assert_eq!(mapped_page(&dst), 1);
    assert!(
        !dst.ops_since(mark).iter().any(|op| matches!(
            op,
            HostOp::Plugin(PluginOp::OnScreen { module, on: false, .. }) if *module == page_id()
        )),
        "shown, it stays on screen"
    );
}

#[test]
fn a_page_still_waiting_for_ready_is_not_moved_again() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    let sent = dst.sent().len();
    dst.move_chat(page_id());
    assert_eq!(dst.toast().as_deref(), Some(feedback::MOVING_TEXT));
    assert_eq!(dst.sent().len(), sent);
    page_ready_event(&dst);
    dst.move_chat(page_id());
    assert!(matches!(dst.last_sent(), Msg::MoveOffer { .. }));
}

#[test]
fn a_refused_adopt_of_a_waiting_page_is_killed_like_any_refused_arrival() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    let timer = dst
        .scheduler
        .armed()
        .into_iter()
        .find(|(_, at)| *at == dst.scheduler.now() + Duration::from_secs(1))
        .unwrap()
        .0;
    dst.driver.post(HostEvent::Refused {
        what: OpTag::AdoptMoved,
        why: "could not restore".into(),
        surface: Some((WIN, page_id())),
    });
    assert!(!dst.scheduler.armed().iter().any(|(t, _)| *t == timer));
    assert!(!dst
        .notes()
        .iter()
        .any(|line| line.contains("its arrival could not be killed")));
    assert!(!dst.away(&page_id()));
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    assert!(matches!(dst.state(&page_id()), Some(ModuleState::Killed(_))));
    let mark = dst.mark();
    page_ready_event(&dst);
    dst.driver.post(HostEvent::Timer(timer));
    assert_eq!(dst.mark(), mark);
    assert_eq!(mapped_page(&dst), 0);
}

#[test]
fn a_waiting_page_is_named_among_what_this_window_holds() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    assert!(crate::window_link::end::held(&dst.layout.borrow(), &dst.workspaces.borrow()).contains(page_id().as_str()));
}

#[test]
fn a_page_shown_by_hand_while_it_waits_is_mapped_at_its_ready() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    dst.driver.post(HostEvent::Lua(LuaEvent::LayoutRequests {
        win: WIN,
        requests: vec![crate::lua::layout::LayoutRequest::Show(page_id())],
    }));
    assert!(dst.layout.borrow().is_shown(&page_id()));
    assert_eq!(mapped_page(&dst), 0);
    let before = page_rectangles(&dst);
    page_ready_event(&dst);
    assert_eq!(mapped_page(&dst), 1);
    assert_eq!(page_rectangles(&dst), before);
    assert!(!dst
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Build { .. }))));
}

#[test]
fn a_page_arriving_with_its_workspace_is_placed_hidden_and_stays_hidden_at_its_ready() {
    let src = page_end(true, false, false);
    src.run(Action::WorkspaceNew);
    src.run(Action::WorkspaceToWindow { follow: false });
    let mut packet = workspace_packet(&src);
    if let Msg::WorkspaceMove { moves, .. } = &mut packet {
        assert_eq!(moves, &[1]);
        moves.push(3);
    }
    let dst = page_end(true, false, true);
    offer_page(&dst, true, true);
    deliver_workspace(&dst, packet);
    dst.peer(PeerEvent::Move {
        peer: PEER,
        id: MoveId(1),
        record: serde_json::json!({"built": true, "state": {"scroll": 12}}),
        fds: Descriptors::default(),
    });
    dst.port.answer(0, Ok(()));
    dst.woke();
    assert!(!dst.away(&page_id()));
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    page_ready_event(&dst);
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    assert_eq!(mapped_page(&dst), 0);
}

#[test]
fn an_arrived_page_is_mapped_only_once_it_is_ready() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    assert_eq!(mapped_page(&dst), 0);
    dst.driver.post(HostEvent::Plugin(PluginEvent::Ready {
        win: WIN,
        module: page_id(),
    }));
    assert_eq!(mapped_page(&dst), 1);
    dst.driver.post(HostEvent::Plugin(PluginEvent::Ready {
        win: WIN,
        module: page_id(),
    }));
    dst.scheduler.advance(Duration::from_secs(2));
    assert_eq!(mapped_page(&dst), 1);
}

#[test]
fn a_page_that_never_says_ready_is_shown_after_a_second() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    let allocations = watch_page_allocations(&dst);
    dst.scheduler.advance(Duration::from_millis(999));
    assert_eq!(mapped_page(&dst), 0);
    dst.scheduler.advance(Duration::from_millis(1));
    assert_eq!(mapped_page(&dst), 1);
    assert!(dst.layout.borrow().is_shown(&page_id()));
    assert_eq!(allocations.borrow().len(), 1);
    assert!(allocations.borrow()[0].0);
    assert!(allocations.borrow()[0].1.iter().any(|(id, _)| *id == page_id()));
    assert!(dst.notes().iter().any(|line| line.contains("ready")));
}

#[test]
fn a_never_built_page_moves_without_a_save_and_is_built_on_arrival_when_shown() {
    let src = page_end(true, true, false);
    src.move_chat(page_id());
    src.ready(1);
    assert!(matches!(src.last_sent(), Msg::Move { record, .. } if record == serde_json::json!({"built": false})));
    assert!(!src
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Freeze { .. }))));
    let dst = page_end(true, true, true);
    commit_page(&dst, false);
    assert!(!dst.ops().iter().any(|op| matches!(op, HostOp::PrepareSurface { .. })));
    assert_eq!(
        dst.count(&HostOp::Plugin(PluginOp::Hold {
            win: WIN,
            module: page_id()
        })),
        1,
        "the window that holds the page answers its commands before it is ever built"
    );
    assert!(!dst.layout.borrow().is_shown(&page_id()));
    assert!(!dst
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Build { .. }))));
    dst.run(Action::Module(page_id()));
    assert_eq!(
        dst.count(&HostOp::Plugin(PluginOp::Build {
            win: WIN,
            module: page_id()
        })),
        1
    );
}

#[test]
fn a_hidden_start_page_is_built_only_by_the_window_holding_it_on_first_show() {
    let src = page_end(true, true, false);
    src.move_chat(page_id());
    src.ready(1);
    src.done(1);
    let dst = page_end(true, true, true);
    commit_page(&dst, false);
    dst.run(Action::Module(page_id()));
    src.run(Action::Module(page_id()));
    assert_eq!(
        src.count(&HostOp::Plugin(PluginOp::Build {
            win: WIN,
            module: page_id()
        })),
        0
    );
    assert_eq!(
        dst.count(&HostOp::Plugin(PluginOp::Build {
            win: WIN,
            module: page_id()
        })),
        1
    );
}

#[test]
fn a_committed_page_survives_the_link_detaching_while_its_document_loads() {
    let dst = page_end(true, false, true);
    commit_page(&dst, true);
    assert_eq!(mapped_page(&dst), 0);
    dst.driver.detach_peer(WIN);
    assert_eq!(mapped_page(&dst), 1);
    dst.scheduler.advance(Duration::from_secs(2));
    assert_eq!(mapped_page(&dst), 1);
}

#[test]
fn a_page_refusal_cannot_give_up_a_terminals_freeze() {
    let src = source();
    src.move_terminal(false);
    src.ready(1);
    src.driver.post(HostEvent::Plugin(PluginEvent::FreezeRefused {
        win: WIN,
        module: page_id(),
        mv: MoveId(1),
        why: "unrelated".into(),
    }));
    assert!(!src.kinds().contains(&"move_abort"));
}

#[test]
fn a_deferred_holding_without_a_leaf_moves_without_being_built() {
    let src = open_with(
        Layout::new(Node::Leaf(agent()), agent()).unwrap(),
        WindowRole::Main,
        EditorPresence::Owned,
        Some(SurfaceDecl {
            module: page_id(),
            kind: SurfaceKind::PluginPage {
                plugin: "markdown".into(),
            },
            deferred: true,
        }),
    );
    assert!(!src.layout.borrow().contains(&page_id()));
    src.move_chat(page_id());
    src.ready(1);
    assert!(matches!(src.last_sent(), Msg::Move { record, .. } if record == serde_json::json!({"built": false})));
    src.done(1);
    assert!(src.away(&page_id()));
    assert!(src
        .workspaces
        .borrow()
        .each(&src.layout.borrow())
        .iter()
        .all(|(_, layout)| layout.is_away(&page_id())));
    src.run(Action::Module(page_id()));
    assert_eq!(
        src.count(&HostOp::Plugin(PluginOp::Build {
            win: WIN,
            module: page_id()
        })),
        0
    );
}

#[test]
fn a_move_ask_finds_an_arrival_by_the_id_the_asking_window_gave_it() {
    // This window holds a terminal of its own, so the other window's `terminal` arrived here as terminal#2.
    let dst = destination_with_a_terminal();
    dst.committed_arrival();
    assert!(
        dst.layout.borrow().contains(&terminal2()),
        "the premise: the arrival took terminal#2"
    );
    // The other window asks for the shell it sent, by the id it had there.
    dst.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: Some(terminal()),
        session: None,
        follow: true,
    });
    let Msg::MoveOffer { mv, module, .. } = dst.last_sent() else {
        panic!("an offer");
    };
    assert_eq!(
        module, "terminal#2",
        "the shell that came from the asking window goes back, not this window's own terminal"
    );

    // Once it has left this window again, an ask for it offers nothing here and asks nothing back.
    dst.ready(mv);
    dst.driver.post(HostEvent::Terminal(TermEvent::Detached {
        win: WIN,
        module: terminal2(),
        mv: MoveId(mv),
        record: serde_json::Value::Null,
        handoff: handoff(),
    }));
    dst.done(mv);
    assert!(dst.away(&terminal2()), "the premise: terminal#2 has left");
    let sent = dst.sent().len();
    dst.peer(PeerEvent::MoveAsk {
        peer: PEER,
        module: Some(terminal()),
        session: None,
        follow: true,
    });
    assert_eq!(dst.sent().len(), sent, "neither an offer nor an ask goes back");
    assert!(dst.layout.borrow().contains(&terminal()) && !dst.away(&terminal()));
}

fn drop_presence(id: u64) -> HostOp {
    HostOp::Chat(ChatOp::DropPresence {
        win: WIN,
        session: SESSION.into(),
        mv: MoveId(id),
    })
}

#[test]
fn the_arriving_sessions_presence_hold_is_let_go_behind_the_commits_adopt() {
    for refused in [false, true] {
        let dst = destination();
        if refused {
            dst.host.refuse_op(|op| matches!(op, HostOp::AdoptMoved { .. }));
        }
        dst.offer_chat(1, agent2());
        dst.deliver(1, Descriptors::new(vec![pipe_fd()]));
        dst.chat(ChatEvent::PresenceHeld {
            win: WIN,
            mv: MoveId(1),
        });
        dst.port.answer(0, Ok(()));
        dst.woke();
        let ops = dst.ops();
        let adopt = ops.iter().position(is_adopt).expect("the commit asks for the adopt");
        let dropped = ops
            .iter()
            .position(|op| *op == drop_presence(1))
            .unwrap_or_else(|| panic!("the hold is let go (adopt refused: {refused})"));
        assert!(
            dropped > adopt,
            "only after the adopt, which takes the hold over onto its tab when it succeeds"
        );
        assert_eq!(dst.count(&drop_presence(1)), 1);
    }
}

#[test]
fn a_chat_whose_place_cannot_be_made_releases_its_held_review_to_the_other_window() {
    // This window runs nvim, so it holds the arriving session's review events from the accepted offer on.
    let dst = chat_source();
    dst.host.refuse_op(|op| matches!(op, HostOp::PrepareSurface { .. }));
    // Window 2 numbers its moves evenly.
    dst.offer_chat(2, agent2());
    assert_eq!(dst.kinds(), vec!["move_refused"], "the premise: the place was refused");
    assert_eq!(dst.count(&review_hold(true)), 1, "held from the accepted offer");
    assert_eq!(
        dst.count(&review_release(false)),
        1,
        "released to the other window, which keeps the chat"
    );
}

fn workspace_packet(end: &End) -> Msg {
    end.sent()
        .into_iter()
        .find_map(|sent| match sent.msg {
            msg @ Msg::WorkspaceMove { .. } => Some(msg),
            _ => None,
        })
        .expect("the workspace has a group")
}

fn deliver_workspace(end: &End, packet: Msg) {
    let Msg::WorkspaceMove {
        group,
        name,
        tree,
        moves,
        swap,
    } = packet
    else {
        panic!("a workspace packet")
    };
    end.peer(PeerEvent::WorkspaceMove {
        peer: PEER,
        group,
        name,
        tree,
        moves,
        swap,
    });
}

fn terminal_workspace() -> End {
    let end = source();
    crate::layout::hide(
        &mut end.layout.borrow_mut(),
        &agent(),
        &crate::layout::Frame::new(GRID, 1),
    )
    .unwrap();
    end.run(Action::WorkspaceNew);
    end
}

#[test]
fn a_workspace_moves_whole_and_the_destination_switches_after_every_module_decided() {
    let layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(agent()),
                Node::split(Axis::Row, 0.5, Node::Leaf(terminal()), Node::Leaf(terminal2())),
            ),
        ),
        terminal(),
    )
    .unwrap();
    let src = open(layout, WindowRole::Main, EditorPresence::Owned);
    crate::layout::hide(
        &mut src.layout.borrow_mut(),
        &agent(),
        &crate::layout::Frame::new(GRID, 1),
    )
    .unwrap();
    src.run(Action::WorkspaceNew);
    let dst = destination();
    let name = src.workspaces.borrow().current().to_owned();
    src.run(Action::WorkspaceToWindow { follow: false });
    let offers: Vec<_> = src
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::MoveOffer { mv, module, .. } => Some((mv, ModuleId::parse(&module).unwrap())),
            _ => None,
        })
        .collect();
    assert_eq!(offers.len(), 2);
    for (id, module) in &offers {
        dst.offer(*id, module.clone(), Shape::Tiled);
    }
    deliver_workspace(&dst, workspace_packet(&src));
    for (ix, (id, module)) in offers.into_iter().enumerate() {
        assert_eq!(dst.workspaces.borrow().current(), "main");
        src.ready(id);
        src.driver.post(HostEvent::Terminal(TermEvent::Detached {
            win: WIN,
            module,
            mv: MoveId(id),
            record: serde_json::json!({}),
            handoff: Handoff::new(Rc::new("terminal")),
        }));
        dst.deliver(id, Descriptors::new(vec![pipe_fd()]));
        dst.port.answer(ix, Ok(()));
        dst.woke();
        src.done(id);
        if ix == 0 {
            assert!(src.workspaces.borrow().order().contains(&name));
        }
    }
    assert_ne!(dst.workspaces.borrow().current(), "main");
    assert!(dst.layout.borrow().is_shown(&terminal()));
    assert!(dst.layout.borrow().is_shown(&terminal2()));
    assert!(!src.workspaces.borrow().order().contains(&name));
    dst.run(Action::WorkspaceSwitch { name: "main".into() });
    for module in [terminal(), terminal2()] {
        assert!(
            !dst.layout.borrow().is_shown(&module),
            "{module} came with the new workspace, not the one window 2 showed before"
        );
    }
}

fn offered_id(end: &End) -> u64 {
    end.sent()
        .iter()
        .find_map(|s| match s.msg {
            Msg::MoveOffer { mv, .. } => Some(mv),
            _ => None,
        })
        .unwrap()
}

#[test]
fn a_moving_workspace_renamed_while_it_waits_is_the_one_that_leaves() {
    let src = terminal_workspace();
    let name = src.workspaces.borrow().current().to_owned();
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    src.run(Action::WorkspaceRename);
    src.driver.post(HostEvent::TopBarEntry {
        win: WIN,
        ev: EntryEv::Enter("study".into()),
    });
    assert_eq!(src.workspaces.borrow().current(), "study");
    src.run(Action::WorkspaceNew);
    assert_eq!(src.workspaces.borrow().current(), name);
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    src.done(id);
    assert!(src.workspaces.borrow().order().contains(&name));
    assert!(!src.workspaces.borrow().order().contains(&"study".into()));
    assert_eq!(src.workspaces.borrow().current(), name);
}

#[test]
fn a_moving_workspace_deleted_while_it_waits_leaves_a_new_one_of_its_name_alone() {
    let src = terminal_workspace();
    let name = src.workspaces.borrow().current().to_owned();
    let index = src.workspaces.borrow().order().iter().position(|n| n == &name).unwrap();
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    src.run(Action::WorkspaceSwitch { name: "main".into() });
    src.run(Action::WorkspaceChoose);
    src.answer("x");
    src.driver.post(HostEvent::KeyReleased { win: WIN, keycode: 42 });
    src.answer(&(index + 1).to_string());
    assert!(!src.workspaces.borrow().order().contains(&name));
    src.run(Action::WorkspaceNew);
    assert_eq!(src.workspaces.borrow().current(), name);
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    src.done(id);
    assert!(src.workspaces.borrow().order().contains(&name));
    assert_eq!(src.workspaces.borrow().current(), name);
}

#[test]
fn an_arriving_terminal_skips_a_number_the_other_window_holds() {
    let dst = destination_with_a_terminal();
    dst.peer(PeerEvent::Held {
        peer: PEER,
        ids: vec![terminal(), terminal2()],
    });
    dst.offer(1, terminal(), Shape::Tiled);
    assert!(dst
        .notes()
        .iter()
        .any(|n| n == "[move] terminal: arriving as terminal#3 (move 1)"));
}

#[test]
fn an_arriving_workspace_keeps_a_hidden_source_terminal_distinct_from_the_arrival() {
    let src = open(
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(agent()),
                    Node::split(Axis::Row, 0.5, Node::Leaf(terminal()), Node::Leaf(terminal2())),
                ),
            ),
            terminal(),
        )
        .unwrap(),
        WindowRole::Main,
        EditorPresence::Owned,
    );
    for module in [agent(), terminal2()] {
        crate::layout::hide(
            &mut src.layout.borrow_mut(),
            &module,
            &crate::layout::Frame::new(GRID, 1),
        )
        .unwrap();
    }
    let dst = destination_with_a_terminal();
    dst.peer(PeerEvent::Held {
        peer: PEER,
        ids: vec![terminal(), terminal2()],
    });
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    deliver_workspace(&dst, workspace_packet(&src));
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    dst.deliver(id, Descriptors::new(vec![pipe_fd()]));
    dst.port.answer(0, Ok(()));
    dst.woke();
    src.done(id);
    assert_ne!(dst.workspaces.borrow().current(), "main");
    assert!(dst.layout.borrow().is_shown(&ModuleId::parse("terminal#3").unwrap()));
    assert!(dst.layout.borrow().is_away(&terminal2()));
    assert!(!dst.layout.borrow().is_shown(&terminal2()));
}

#[test]
fn an_arriving_workspace_keeps_window_1s_ended_editor_off_screen() {
    let dst = open(
        Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
            agent(),
        )
        .unwrap(),
        WindowRole::Main,
        EditorPresence::Owned,
    );
    dst.driver.start(
        WIN,
        crate::wm::start::StartStep::Feeds {
            spawn: EditorSpawn {
                env: vec![],
                args: vec![],
                cwd: None,
                clean: true,
                background: None,
                open: vec![],
            },
        },
    );
    dst.host.set_chat_kill_counts(
        ViewId::BASE,
        ChatKillCounts {
            running: 1,
            ..Default::default()
        },
    );
    dst.host.set_editor_facts(EditorFacts {
        exited: true,
        ..Default::default()
    });
    dst.driver.post(HostEvent::Editor(EditorEvent::Exited {
        win: WIN,
        settled: None,
    }));
    dst.answer("n");
    dst.scheduler.advance(Duration::ZERO);
    dst.driver.post(HostEvent::Editor(EditorEvent::Released { win: WIN }));
    assert!(!dst.layout.borrow().is_shown(&editor()));
    let src = open(
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(editor()),
                Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
            ),
            terminal(),
        )
        .unwrap(),
        WindowRole::Linked { side: LinkSide::Second },
        EditorPresence::Proxy,
    );
    crate::layout::hide(
        &mut src.layout.borrow_mut(),
        &agent(),
        &crate::layout::Frame::new(GRID, 1),
    )
    .unwrap();
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    deliver_workspace(&dst, workspace_packet(&src));
    let mark = dst.mark();
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    dst.deliver(id, Descriptors::new(vec![pipe_fd()]));
    dst.port.answer(0, Ok(()));
    dst.woke();
    src.done(id);
    assert_ne!(dst.workspaces.borrow().current(), "main");
    assert!(!dst.layout.borrow().is_shown(&editor()));
    assert!(!dst
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::Map { module, .. } if module == &editor())));
    let mark = dst.mark();
    dst.run(Action::Module(editor()));
    assert!(dst
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::Editor(EditorCall::Restart { .. }))));
}

#[test]
fn the_link_ending_mid_workspace_move_shows_the_arrivals_that_committed() {
    let src = open(
        Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(terminal()), Node::Leaf(terminal2())),
            terminal(),
        )
        .unwrap(),
        WindowRole::Main,
        EditorPresence::Owned,
    );
    let dst = destination();
    src.run(Action::WorkspaceToWindow { follow: false });
    let offers: Vec<_> = src
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::MoveOffer { mv, module, .. } => Some((mv, ModuleId::parse(&module).unwrap())),
            _ => None,
        })
        .collect();
    assert_eq!(offers.len(), 2);
    for (id, module) in &offers {
        dst.offer(*id, module.clone(), Shape::Tiled);
    }
    deliver_workspace(&dst, workspace_packet(&src));
    let (id, module) = &offers[0];
    src.ready(*id);
    src.driver.post(HostEvent::Terminal(TermEvent::Detached {
        win: WIN,
        module: module.clone(),
        mv: MoveId(*id),
        record: serde_json::json!({}),
        handoff: Handoff::new(Rc::new("terminal")),
    }));
    dst.deliver(*id, Descriptors::new(vec![pipe_fd()]));
    dst.port.answer(0, Ok(()));
    dst.woke();
    src.done(*id);
    assert!(dst.layout.borrow().contains(module));
    assert!(!dst.layout.borrow().is_shown(module));
    let mark = dst.mark();
    dst.link_closed();
    assert!(dst.layout.borrow().is_shown(module));
    assert_eq!(dst.workspaces.borrow().current(), "main");
    assert!(!dst.layout.borrow().is_shown(&offers[1].1));
    assert!(dst
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::Map { module: mapped, .. } if mapped == module)));
}

#[test]
fn a_refusing_module_stays_and_is_away_in_the_destinations_copy() {
    let src = terminal_workspace();
    let dst = destination();
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    deliver_workspace(&dst, workspace_packet(&src));
    dst.peer(PeerEvent::MoveAbort {
        peer: PEER,
        id: MoveId(id),
        why: "cannot freeze".into(),
    });
    src.peer(PeerEvent::MoveRefused {
        peer: PEER,
        id: MoveId(id),
        why: "cannot freeze".into(),
    });
    assert!(dst.layout.borrow().is_away(&terminal()));
    assert!(!src.layout.borrow().is_away(&terminal()));
    assert!(src.toast().unwrap().contains("cannot freeze"));
    assert_ne!(src.workspaces.borrow().current(), "main");
}

#[test]
fn moving_main_leaves_window_1s_main_with_the_editor() {
    let src = terminal_workspace();
    src.run(Action::WorkspaceSwitch { name: "main".into() });
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    src.done(id);
    assert!(src.workspaces.borrow().order().contains(&"main".into()));
    assert!(src.layout.borrow().is_visible(&editor()));
    assert!(!src.layout.borrow().is_away(&editor()));
}

#[test]
fn a_name_taken_in_the_destination_gets_the_lowest_free_number() {
    let dst = destination();
    let tree: serde_json::Value = serde_json::from_str(&crate::layout::persist::encode_v2(
        &Workspaces::single(),
        &source().layout.borrow(),
        std::path::Path::new("/workspace"),
    ))
    .unwrap();
    let mut tree = tree["workspaces"][0].clone();
    tree["name"] = "main".into();
    dst.peer(PeerEvent::WorkspaceMove {
        peer: PEER,
        group: 99,
        name: "main".into(),
        tree,
        moves: vec![],
        swap: None,
    });
    assert_eq!(dst.workspaces.borrow().current(), "main 2");
}

#[test]
fn peer_workspace_names_and_trees_are_validated_before_layout_changes() {
    let dst = destination();
    let before = dst.layout.borrow().clone();
    for name in ["bad\nname".to_owned(), "x".repeat(33), "".into()] {
        dst.peer(PeerEvent::WorkspaceMove {
            peer: PEER,
            group: 99,
            name,
            tree: serde_json::json!({}),
            moves: vec![],
            swap: None,
        });
        assert_eq!(*dst.layout.borrow(), before);
        assert_eq!(dst.workspaces.borrow().current(), "main");
    }
}

#[test]
fn a_swap_switches_both_windows_at_once_and_the_editor_stays_in_window_1() {
    let src = terminal_workspace();
    let dst = destination();
    src.run(Action::WorkspaceSwapWindow);
    assert!(src.kinds().contains(&"swap"));
    let Msg::Swap { move_a, move_b } = src.last_sent() else {
        panic!("the swap request follows its group")
    };
    deliver_workspace(&dst, workspace_packet(&src));
    dst.peer(PeerEvent::Swap {
        peer: PEER,
        move_a,
        move_b,
    });
    assert!(dst.kinds().contains(&"workspace_move"));
    assert_eq!(dst.workspaces.borrow().current(), "main");
    let response = dst
        .sent()
        .into_iter()
        .find_map(|s| match s.msg {
            Msg::Swap { move_a, move_b } => Some((move_a, move_b)),
            _ => None,
        })
        .unwrap();
    src.peer(PeerEvent::Swap {
        peer: PEER,
        move_a: response.0,
        move_b: response.1,
    });
    deliver_workspace(&src, workspace_packet(&dst));
    assert_eq!(dst.workspaces.borrow().current(), "main");
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    dst.deliver(id, Descriptors::new(vec![pipe_fd()]));
    dst.port.answer(0, Ok(()));
    dst.woke();
    src.done(id);
    assert_ne!(dst.workspaces.borrow().current(), "main");
    assert_ne!(src.workspaces.borrow().current(), "main");
    let layout = src.layout.borrow();
    assert!(
        !layout.is_gone(&editor()),
        "window 1's editor is never retired by an arriving workspace"
    );
    assert!(
        layout.is_visible(&editor()),
        "and stays on screen in the workspace window 1 now shows"
    );
}

#[test]
fn a_swap_whose_other_half_cannot_start_moves_this_half_alone() {
    let src = terminal_workspace();
    let dst = destination();
    // More base tabs than one workspace may move, so window 2's half is refused before it starts.
    dst.host.set_base_tabs(
        (1..=300)
            .map(|n| ChatMoveFacts {
                tab: TabId(n),
                session: None,
                live: true,
                running: false,
            })
            .collect(),
        Some(TabId(1)),
    );
    src.run(Action::WorkspaceSwapWindow);
    let Msg::Swap { move_a, move_b } = src.last_sent() else {
        panic!("the swap request follows its group")
    };
    deliver_workspace(&dst, workspace_packet(&src));
    dst.peer(PeerEvent::Swap {
        peer: PEER,
        move_a,
        move_b,
    });
    assert!(!dst.kinds().contains(&"workspace_move"));
    assert!(
        dst.sent()
            .iter()
            .any(|s| matches!(s.msg, Msg::Swap { move_a: a, move_b: 0 } if a == move_a)),
        "no half will come"
    );
    src.peer(PeerEvent::Swap {
        peer: PEER,
        move_a,
        move_b: 0,
    });
    assert!(src.toast().unwrap().contains("moves alone"));
    let name = src.workspaces.borrow().current().to_owned();
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    src.ready(id);
    src.detached(id, serde_json::json!({}), Handoff::new(Rc::new("terminal")));
    dst.deliver(id, Descriptors::new(vec![pipe_fd()]));
    dst.port.answer(0, Ok(()));
    dst.woke();
    src.done(id);
    assert_ne!(
        dst.workspaces.borrow().current(),
        "main",
        "window 2 shows the workspace that came"
    );
    assert!(
        !src.workspaces.borrow().order().contains(&name),
        "and window 1 no longer has it"
    );
}

#[test]
fn a_workspace_packet_that_fails_to_read_drops_the_arrivals_it_named() {
    let src = terminal_workspace();
    let dst = destination();
    src.run(Action::WorkspaceToWindow { follow: false });
    let id = offered_id(&src);
    dst.offer(id, terminal(), Shape::Tiled);
    let Msg::WorkspaceMove { group, tree, moves, .. } = workspace_packet(&src) else {
        unreachable!()
    };
    dst.peer(PeerEvent::WorkspaceMove {
        peer: PEER,
        group,
        name: "bad\nname".into(),
        tree,
        moves,
        swap: None,
    });
    assert!(dst
        .sent()
        .iter()
        .any(|s| matches!(&s.msg, Msg::MoveAbort { mv, .. } if *mv == id)));
    assert!(
        dst.ops().iter().any(|op| matches!(op, HostOp::DropSurface { .. })),
        "the place made for the arrival is dropped with it"
    );
    src.peer(PeerEvent::MoveAbort {
        peer: PEER,
        id: MoveId(id),
        why: "bad name".into(),
    });
    assert!(src.toast().unwrap().contains("stayed"));
    assert!(!src.layout.borrow().is_away(&terminal()));
}

fn base_workspace() -> End {
    let src = source();
    crate::layout::hide(
        &mut src.layout.borrow_mut(),
        &terminal(),
        &crate::layout::Frame::new(GRID, 1),
    )
    .unwrap();
    src.host.set_base_tabs(
        vec![
            ChatMoveFacts {
                tab: TabId(4),
                session: None,
                live: true,
                running: false,
            },
            ChatMoveFacts {
                tab: TabId(5),
                session: None,
                live: true,
                running: false,
            },
        ],
        Some(TabId(4)),
    );
    src
}

fn arrive_base_group(src: &End, dst: &End) {
    let offers: Vec<_> = src
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::MoveOffer { mv, module, .. } => Some((mv, ModuleId::parse(&module).unwrap())),
            _ => None,
        })
        .collect();
    for (id, module) in &offers {
        dst.offer_chat(*id, module.clone());
    }
    deliver_workspace(dst, workspace_packet(src));
    for (id, _) in offers.into_iter().rev() {
        src.ready(id);
        src.chat_frozen(ViewId::BASE, id);
        let record = src
            .sent()
            .into_iter()
            .find_map(|s| match s.msg {
                Msg::Move { mv, record } if mv == id => Some(record),
                _ => None,
            })
            .unwrap();
        dst.peer(PeerEvent::Move {
            peer: PEER,
            id: MoveId(id),
            record,
            fds: Descriptors::default(),
        });
        dst.chat(ChatEvent::PresenceHeld {
            win: WIN,
            mv: MoveId(id),
        });
        let ix = dst.port.sent().iter().filter(|s| s.confirmed).count() - 1;
        dst.port.answer(ix, Ok(()));
        dst.woke();
        src.done(id);
    }
}

#[test]
fn a_first_move_into_a_fresh_window_2_leaves_no_empty_tab_in_front() {
    let src = base_workspace();
    let dst = destination();
    dst.host.set_base_tabs(
        vec![ChatMoveFacts {
            tab: TabId(1),
            session: None,
            live: true,
            running: false,
        }],
        Some(TabId(1)),
    );
    src.run(Action::WorkspaceToWindow { follow: false });
    arrive_base_group(&src, &dst);
    assert_eq!(dst.host.base_tabs(WIN), vec![TabId(1001), TabId(1003)]);
    assert_eq!(dst.host.active_base_tab(WIN), Some(TabId(1001)));
}

#[test]
fn a_round_trip_leaves_both_base_views_as_they_were() {
    let src = base_workspace();
    let dst = destination();
    dst.host.set_base_tabs(
        vec![ChatMoveFacts {
            tab: TabId(9),
            session: None,
            live: true,
            running: false,
        }],
        Some(TabId(9)),
    );
    src.run(Action::WorkspaceToWindow { follow: false });
    arrive_base_group(&src, &dst);
    assert_eq!(dst.host.base_tabs(WIN), vec![TabId(9), TabId(1001), TabId(1003)]);
    dst.run(Action::WorkspaceToWindow { follow: false });
    let offered: Vec<_> = dst
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            Msg::MoveOffer { mv, .. } => Some(mv),
            _ => None,
        })
        .collect();
    assert_eq!(offered.len(), 2, "only the tabs recorded for the workspace go back");
    arrive_base_group(&dst, &src);
    assert_eq!(dst.host.base_tabs(WIN), vec![TabId(9)]);
    assert_eq!(src.host.base_tabs(WIN).len(), 2);
    assert_ne!(src.host.active_base_tab(WIN), Some(TabId(1)));
}

#[test]
fn a_tab_whose_turn_cannot_move_live_stays_and_the_others_move_at_once() {
    let src = base_workspace();
    src.host.set_base_tabs(
        vec![
            ChatMoveFacts {
                tab: TabId(4),
                session: None,
                live: false,
                running: true,
            },
            ChatMoveFacts {
                tab: TabId(5),
                session: None,
                live: true,
                running: false,
            },
        ],
        Some(TabId(4)),
    );
    src.run(Action::WorkspaceToWindow { follow: false });
    assert!(src.kinds().contains(&"workspace_move"), "a workspace waits for no turn");
    let dst = destination();
    arrive_base_group(&src, &dst);
    assert_eq!(dst.host.base_tabs(WIN).len(), 1, "only the idle tab came");
    let toast = src.toast().unwrap();
    assert!(
        toast.contains("agent tab 4 stayed") && toast.contains(crate::wm::feedback::TURN_RUNS_STAYS_TEXT),
        "{toast}"
    );
}

#[test]
fn a_workspace_move_announces_why_a_tab_stayed_when_the_group_settles() {
    let src = base_workspace();
    src.host.set_base_tabs(
        vec![
            ChatMoveFacts {
                tab: TabId(4),
                session: None,
                live: false,
                running: true,
            },
            ChatMoveFacts {
                tab: TabId(5),
                session: None,
                live: true,
                running: false,
            },
        ],
        Some(TabId(4)),
    );
    src.run(Action::WorkspaceToWindow { follow: false });
    let announced_toast = Rc::new(RefCell::new(None));
    {
        let driver = Rc::downgrade(&src.driver);
        let announced_toast = announced_toast.clone();
        src.host.on_apply(move |_, _, op| {
            if matches!(op, HostOp::Chrome { win: WIN, .. }) {
                *announced_toast.borrow_mut() = driver.upgrade().unwrap().query(|c| c.chrome(WIN).toast);
            }
        });
    }
    let dst = destination();
    arrive_base_group(&src, &dst);
    assert_eq!(dst.host.base_tabs(WIN).len(), 1, "only the idle tab came");
    let toast = src.driver.query(|c| c.chrome(WIN).toast).unwrap();
    assert!(
        toast.text.contains("agent tab 4 stayed") && toast.text.contains(crate::wm::feedback::TURN_RUNS_STAYS_TEXT),
        "{}",
        toast.text
    );
    assert_eq!(
        announced_toast.borrow().as_ref(),
        Some(&toast),
        "the final Chrome op must carry the post-drain toast and its serial before the driver stops draining"
    );
}

#[test]
fn a_workspace_deleted_after_its_round_trip_leaves_no_record_for_a_new_one_of_its_name() {
    let src = base_workspace();
    let dst = destination();
    dst.host.set_base_tabs(
        vec![ChatMoveFacts {
            tab: TabId(9),
            session: None,
            live: true,
            running: false,
        }],
        Some(TabId(9)),
    );
    src.run(Action::WorkspaceToWindow { follow: false });
    arrive_base_group(&src, &dst);
    let name = dst.workspaces.borrow().current().to_owned();
    dst.run(Action::WorkspaceToWindow { follow: false });
    arrive_base_group(&dst, &src);
    assert!(!dst.workspaces.borrow().order().contains(&name));
    let mark = dst.sent().len();
    dst.run(Action::WorkspaceSwitch { name: name.clone() });
    assert_eq!(dst.workspaces.borrow().current(), name);
    dst.run(Action::WorkspaceToWindow { follow: false });
    let offered = dst.sent()[mark..]
        .iter()
        .filter(|s| matches!(s.msg, Msg::MoveOffer { .. }))
        .count();
    assert_eq!(
        offered, 1,
        "the new workspace moves window 2's own tab, as any workspace with no record does"
    );
}
