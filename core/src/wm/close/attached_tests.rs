//! The close of a window over a person's own nvim: it is let go of, never quit, ended or restarted.

use super::*;
use crate::keymap::{prefix::Press, Action, KeySpec};
use crate::layout::{Axis, Layout, Node};
use crate::lua::layout::LayoutRequest;
use crate::tabs::ViewId;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::feedback::{unsaved_in_editor_prompt, BLOCKING_PROMPT, GIVE_BACK_PROMPT, LINK_LOST_TEXT};
use crate::wm::host::*;
use crate::wm::verbs::rig::{decl, rig_with, WIN};
use crate::wm::window::EditorKind;
use crate::wm::Driver;
use std::rc::Rc;

struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
}

/// A window over a person's own nvim, holding `layout`, with every module hosted.
fn attached(layout: Layout) -> Rig {
    attached_with(layout, |_| {})
}

/// As [`attached`], with `before` run on the window before the driver takes the controller.
fn attached_with(layout: Layout, before: impl FnOnce(&mut Window)) -> Rig {
    rig_of(layout, EditorKind::Attached, before)
}

/// A window over an nvim it started itself, holding `layout`, with every module hosted.
fn owned(layout: Layout) -> Rig {
    rig_of(layout, EditorKind::Owned, |_| {})
}

fn rig_of(layout: Layout, kind: EditorKind, before: impl FnOnce(&mut Window)) -> Rig {
    let mut r = rig_with(layout, |_, _| None);
    r.controller.set_editor_kind(WIN, kind);
    before(r.controller.window_mut(WIN).unwrap());
    let hosted: Vec<_> = r.layout.borrow().modules().iter().map(decl).collect();
    for d in hosted {
        r.host.with_surface(WIN, d.module, d.kind, false);
    }
    let driver = Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    r.host.attach(Rc::downgrade(&sink));
    r.scheduler.attach(Rc::downgrade(&sink));
    for op in r.first.ops() {
        r.host.apply(op);
    }
    Rig {
        driver,
        host: r.host,
        scheduler: r.scheduler,
    }
}

fn two() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        ),
        ModuleId::editor(),
    )
    .unwrap()
}

fn editor_alone() -> Layout {
    Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap()
}

fn terminal_alone() -> Layout {
    Layout::new(Node::Leaf(ModuleId::terminal()), ModuleId::terminal()).unwrap()
}

/// What may never reach a person's own nvim: a quit, an end, a restart, the owned editor's shutdown, or the reopen
/// record's calls.
fn ends_or_records(call: &EditorCall) -> bool {
    matches!(
        call,
        EditorCall::Quit { .. }
            | EditorCall::End { .. }
            | EditorCall::Restart { .. }
            | EditorCall::Shutdown { .. }
            | EditorCall::Capture { .. }
            | EditorCall::ReadHandoff { .. }
            | EditorCall::DropInput { .. }
    )
}

impl Rig {
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
    fn ops_since(&self, at: usize) -> Vec<HostOp> {
        self.ops().split_off(at)
    }
    fn editor_calls(&self) -> Vec<EditorCall> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                HostOp::Editor(call) => Some(call),
                _ => None,
            })
            .collect()
    }
    fn count(&self, which: impl Fn(&HostOp) -> bool) -> usize {
        self.ops().iter().filter(|op| which(op)).count()
    }
    fn detaches(&self) -> usize {
        self.count(|op| matches!(op, HostOp::Editor(EditorCall::Detach { win: WIN })))
    }
    fn fact_asks(&self) -> usize {
        self.count(|op| matches!(op, HostOp::Editor(EditorCall::AskCloseFacts { win: WIN, .. })))
    }
    /// The generation of the last ask for the editor's facts.
    fn last_ask(&self) -> u32 {
        self.ops()
            .iter()
            .rev()
            .find_map(|op| match op {
                HostOp::Editor(EditorCall::AskCloseFacts { win: WIN, generation }) => Some(*generation),
                _ => None,
            })
            .expect("the editor was asked for its facts")
    }
    fn closes(&self) -> usize {
        self.count(|op| matches!(op, HostOp::CloseWindow { win: WIN }))
    }
    fn prompt(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).prompt.map(|prompt| prompt.text))
    }
    fn toast(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).toast.map(|toast| toast.text))
    }
    fn running(&self) {
        self.host.set_chat_kill_counts(
            ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
    }
    fn answer(&self, key: &str) {
        let spec = KeySpec::parse(key).unwrap();
        self.driver.key(
            WIN,
            &KeyStage::Window,
            KeyPress {
                keycode: 42,
                press: Press::Key(spec),
                key: Some(spec.key),
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
    }
    /// The answer to the last ask for the editor's facts.
    fn facts(&self, asks: CloseAsks) {
        self.facts_for(self.last_ask(), asks);
    }
    fn facts_for(&self, generation: u32, asks: CloseAsks) {
        self.driver.post(HostEvent::Editor(EditorEvent::CloseFacts {
            win: WIN,
            generation,
            asks,
        }));
    }
    /// The editor says it let go of the nvim.
    fn detached(&self) {
        self.driver.post(HostEvent::Editor(EditorEvent::Detached { win: WIN }));
    }
    fn exited(&self) {
        self.driver.post(HostEvent::Editor(EditorEvent::Exited {
            win: WIN,
            settled: None,
        }));
    }
    fn state(&self) -> CloseState {
        self.driver
            .query(|c| c.window(WIN).map(|w| w.close.state).unwrap_or_default())
    }
    /// Whether the editor is on screen, and whether it holds the keys.
    fn editor_shown(&self) -> (bool, bool) {
        self.driver.query(|c| {
            c.window(WIN)
                .map(|w| {
                    let layout = w.layout.borrow();
                    (
                        layout.is_shown(&ModuleId::editor()),
                        *layout.focus() == ModuleId::editor(),
                    )
                })
                .unwrap_or_default()
        })
    }
    fn hand_back(&self, why: HandBackWhy) {
        self.driver
            .post(HostEvent::Editor(EditorEvent::HandedBack { win: WIN, why }));
    }
    fn advance(&self, by: Duration) {
        self.scheduler.advance(by);
    }
    /// Carries a close through to its end: every question answered `y`, the waits for the facts and for the
    /// detach's answer and the idles run out.
    fn finish(&self) {
        for _ in 0..8 {
            if self.prompt().is_some() {
                self.answer("y");
            }
            self.advance(CLOSE_FACTS_WITHIN);
            self.advance(DETACHED_WITHIN);
            self.advance(Duration::ZERO);
        }
    }
    fn assert_nothing_ends_or_records(&self, case: &str) {
        let wrong: Vec<_> = self.editor_calls().into_iter().filter(ends_or_records).collect();
        assert_eq!(wrong, Vec::<EditorCall>::new(), "{case}");
    }
}

/// One row of the close table: its name, the layout, what starts the close, and how often nvim is let go of.
type Case = (&'static str, fn() -> Layout, Box<dyn Fn(&Rig)>, usize);

fn case(name: &'static str, layout: fn() -> Layout, run: impl Fn(&Rig) + 'static) -> Case {
    (name, layout, Box::new(run), 1)
}

fn verb(name: &'static str, run: impl Fn(&Rig) + 'static) -> (&'static str, Box<dyn Fn(&Rig)>) {
    (name, Box::new(run))
}

const WHYS: [HandBackWhy; 6] = [
    HandBackWhy::OtherUi,
    HandBackWhy::TakeBack,
    HandBackWhy::LinkLost,
    HandBackWhy::Replaced,
    HandBackWhy::AttachFailed,
    HandBackWhy::Connected,
];

/// Every way a window closes, and every verb that would end or restart its editor, each on a window over a person's
/// own nvim: what reaches the editor is a `Detach`, an `AskCloseFacts` or nothing, never a quit, an end, a restart, a
/// shutdown or the record's calls. A new cause or verb joins this table and takes a side.
#[test]
fn an_attached_editor_never_gets_quit_end_restart_or_shutdown() {
    let mut cases: Vec<Case> = vec![
        case("the top bar's close", two, |r| {
            r.driver.close_requested(WIN, CloseBy::User);
        }),
        case("the compositor's close", two, |r| {
            r.driver.close_requested(WIN, CloseBy::Compositor);
        }),
        case("a termination signal", two, |r| {
            r.driver.post(HostEvent::EndSignal { name: "SIGTERM" })
        }),
        case("the group's close", two, |r| {
            r.driver.post(HostEvent::GroupClosed { win: WIN })
        }),
        case("the last terminal's exit", terminal_alone, |r| {
            r.driver.post(HostEvent::Terminal(TermEvent::Exited {
                win: WIN,
                module: ModuleId::terminal(),
            }))
        }),
        case("prefix x on the editor", two, |r| {
            r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 })
        }),
        case("prefix x on the editor, the last module", editor_alone, |r| {
            r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 })
        }),
        case("the editor's kill with no question", two, |r| {
            r.driver.kill_confirmed(WIN, &ModuleId::editor())
        }),
    ];
    for why in WHYS {
        cases.push(case("a hand-back", two, move |r| r.hand_back(why)));
    }
    // nvim's own end leaves nothing to let go of.
    let mut ended = case("nvim's own end", two, |r| {
        r.driver.post(HostEvent::Editor(EditorEvent::Exited {
            win: WIN,
            settled: None,
        }))
    });
    ended.3 = 0;
    cases.push(ended);
    // The verbs that start an ended editor again, after the nvim was let go of: none starts one.
    let restarts: Vec<(&str, Box<dyn Fn(&Rig)>)> = vec![
        verb("prefix e", |r| {
            r.driver
                .run(WIN, Action::Module(ModuleId::editor()), Origin::Key { keycode: 7 })
        }),
        verb("the editor's chip", |r| {
            r.driver.run(WIN, Action::Module(ModuleId::editor()), Origin::Chip)
        }),
        verb("Lua's show", |r| {
            crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Show(ModuleId::editor()))
        }),
        verb("Lua's focus", |r| {
            crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Focus(ModuleId::editor()))
        }),
        verb("a plugin's follow", |r| r.driver.follow(WIN, &ModuleId::editor(), true)),
        verb("the review's open", |r| {
            r.driver.post(HostEvent::Chat(ChatEvent::ReviewOpen { win: WIN }))
        }),
        verb("an open from another process", |r| {
            r.driver.post(HostEvent::OpenFile {
                win: WIN,
                id: 1,
                path: "/project/file.rs".into(),
                line: None,
                col: None,
                token: None,
            })
        }),
    ];
    for (name, layout, case, detaches) in cases {
        let r = attached(layout());
        case(&r);
        r.finish();
        r.assert_nothing_ends_or_records(name);
        assert_eq!(
            r.detaches(),
            detaches,
            "{name}: let go of once, or not at all once nvim ended"
        );
        assert_eq!(r.closes(), 1, "{name}: the window closed");
    }
    for (name, verb) in restarts {
        let r = attached(two());
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(ModuleId::editor()));
        r.driver.post(HostEvent::Editor(EditorEvent::Detached { win: WIN }));
        verb(&r);
        r.assert_nothing_ends_or_records(name);
        assert_eq!(r.detaches(), 0, "{name}: nothing let go of twice");
        assert_eq!(r.closes(), 0, "{name}: the window stays");
    }
}

#[test]
fn prefix_x_on_an_attached_editor_asks_then_closes() {
    let r = attached(two());
    let at = r.ops().len();
    r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
    assert_eq!(r.prompt().as_deref(), Some(GIVE_BACK_PROMPT));
    assert_eq!(
        r.ops_since(at),
        Vec::<HostOp>::new(),
        "nothing reaches nvim before the answer"
    );
    r.answer("y");
    assert_eq!(
        r.ops_since(at),
        [HostOp::Editor(EditorCall::AskCloseFacts {
            win: WIN,
            generation: 1
        })],
        "the `y` is a close: it asks the editor first"
    );
    r.facts(CloseAsks::default());
    assert_eq!(r.detaches(), 1);
    assert_eq!(r.prompt(), None);
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
    assert_eq!(r.detaches(), 1, "the teardown does not let go again");
    r.assert_nothing_ends_or_records("prefix x");

    // `n` keeps everything as it was.
    let r = attached(two());
    r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
    let at = r.ops().len();
    r.answer("n");
    r.finish();
    assert_eq!(r.prompt(), None);
    assert_eq!(r.ops_since(at), Vec::<HostOp>::new());
    assert_eq!(r.closes(), 0);
}

#[test]
fn a_close_waits_for_close_facts_then_asks_drafts_and_proposals_then_blocking_then_detaches() {
    // With an answer.
    let r = attached(two());
    r.running();
    let at = r.ops().len();
    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(
        r.ops_since(at),
        [HostOp::Editor(EditorCall::AskCloseFacts {
            win: WIN,
            generation: 1
        })]
    );
    assert_eq!(r.prompt(), None, "nothing is asked before the editor answers");
    r.advance(CLOSE_FACTS_WITHIN - Duration::from_millis(1));
    assert_eq!(r.prompt(), None, "the wait is the close's mode ask, 200 ms");
    r.facts(CloseAsks {
        blocking: true,
        drafts: 1,
        proposals: 2,
        ..CloseAsks::default()
    });
    assert_eq!(
        r.prompt().as_deref(),
        Some("the draft in the editor has unsaved changes, 2 agent proposals are open, close anyway? (y/n)")
    );
    assert_eq!(r.prompt(), Some(unsaved_in_editor_prompt(1, 2)));
    r.answer("y");
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    assert_eq!(r.detaches(), 0, "nothing is let go of while a question about it is up");
    r.answer("y");
    assert_eq!(r.detaches(), 1);
    // The turns are asked about after the nvim is back with its terminal.
    let detach_at = r
        .ops()
        .iter()
        .position(|op| matches!(op, HostOp::Editor(EditorCall::Detach { .. })))
        .unwrap();
    assert_eq!(r.prompt(), crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0));
    assert_eq!(r.closes(), 0);
    r.detached();
    r.answer("y");
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
    assert!(r.ops()[detach_at + 1..]
        .iter()
        .all(|op| !matches!(op, HostOp::Editor(EditorCall::Detach { .. }))));
    r.assert_nothing_ends_or_records("answered");

    // A `n` to either question keeps nvim, and a later close asks again.
    for decline_at in 0..2 {
        let r = attached(two());
        r.driver.close_requested(WIN, CloseBy::User);
        r.facts(CloseAsks {
            blocking: true,
            drafts: 1,
            proposals: 0,
            ..CloseAsks::default()
        });
        assert_eq!(r.prompt(), Some(unsaved_in_editor_prompt(1, 0)));
        if decline_at == 1 {
            r.answer("y");
            assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
        }
        r.answer("n");
        assert_eq!(r.prompt(), None);
        r.advance(Duration::from_secs(1));
        assert_eq!((r.detaches(), r.closes()), (0, 0), "declined at question {decline_at}");
        r.driver.close_requested(WIN, CloseBy::User);
        assert_eq!(r.fact_asks(), 2, "the next close asks the editor again");
    }

    // With the 200 ms timeout: no answer counts as nvim waiting for a key.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.advance(CLOSE_FACTS_WITHIN - Duration::from_millis(1));
    assert_eq!(r.prompt(), None);
    r.advance(Duration::from_millis(1));
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    // An answer that comes after the bound changes nothing.
    r.facts(CloseAsks {
        blocking: false,
        drafts: 3,
        proposals: 0,
        ..CloseAsks::default()
    });
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    assert_eq!(r.detaches(), 0);
    r.answer("y");
    assert_eq!(r.detaches(), 1);
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
    r.assert_nothing_ends_or_records("timed out");

    // Nothing to ask: let go of at once, and closed from an idle.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks::default());
    assert_eq!((r.prompt(), r.detaches(), r.closes()), (None, 1, 0));
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
}

#[test]
fn handed_back_detaches_without_a_question_and_asks_only_about_running_turns() {
    for why in WHYS {
        let r = attached(two());
        r.running();
        let at = r.ops().len();
        r.hand_back(why);
        assert_eq!(
            r.ops_since(at),
            [HostOp::Editor(EditorCall::Detach { win: WIN })],
            "{why:?}: let go of at once, nothing asked of the editor"
        );
        assert_eq!(
            r.prompt(),
            crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0),
            "{why:?}"
        );
        r.detached();
        r.answer("y");
        r.advance(Duration::ZERO);
        assert_eq!((r.detaches(), r.closes(), r.fact_asks()), (1, 1, 0), "{why:?}");
        r.assert_nothing_ends_or_records("handed back");
    }

    // With no turn running nothing is asked at all.
    let r = attached(two());
    r.hand_back(HandBackWhy::OtherUi);
    assert_eq!(r.prompt(), None);
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!((r.detaches(), r.closes()), (1, 1));

    // A lost link says so.
    let r = attached(two());
    r.hand_back(HandBackWhy::LinkLost);
    assert_eq!(r.toast().as_deref(), Some(LINK_LOST_TEXT));

    // A hand-back while a question about the editor is up takes its place: nvim is let go of now.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.advance(CLOSE_FACTS_WITHIN);
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    r.hand_back(HandBackWhy::TakeBack);
    assert_eq!(r.detaches(), 1);
    assert_eq!(r.prompt(), None);
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);

    // A hand-back while the facts are awaited stops the wait.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.hand_back(HandBackWhy::OtherUi);
    r.facts(CloseAsks {
        blocking: true,
        drafts: 1,
        proposals: 1,
        ..CloseAsks::default()
    });
    assert_eq!(r.prompt(), None, "the late facts ask nothing");
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!((r.detaches(), r.closes()), (1, 1));
}

#[test]
fn the_signal_close_detaches_an_attached_editor() {
    let r = attached(two());
    r.running();
    let at = r.ops().len();
    r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    assert_eq!(
        r.ops_since(at),
        [
            HostOp::Cleanup(CleanupOp::PairOnClose { layout_written: false }),
            HostOp::Editor(EditorCall::Detach { win: WIN }),
            HostOp::Terminal(TermOp::ShutdownAll { win: WIN }),
            HostOp::Cleanup(CleanupOp::PaneSwitchDir),
            HostOp::Cleanup(CleanupOp::Feeds),
            HostOp::Plugin(PluginOp::ShutdownAll { win: WIN }),
            HostOp::Chat(ChatOp::ShutdownAll { win: WIN, hold: true }),
            HostOp::Cleanup(CleanupOp::ScratchDir),
            HostOp::CloseWindow { win: WIN },
        ],
        "the owned editor's teardown, with `Editor(Detach)` in place of `Editor(Shutdown)` and no record"
    );
    assert_eq!(r.prompt(), None, "a signal asks nothing");

    // A signal after nvim was let go of lets go of nothing again.
    let r = attached(two());
    r.hand_back(HandBackWhy::OtherUi);
    r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    assert_eq!((r.detaches(), r.closes()), (1, 1));
}

/// A signal and a hand-back run whatever the close is doing: the nvim is let go of at once, and a signal closes the
/// window at once.
#[test]
fn handed_back_and_signals_are_always_run() {
    for why in WHYS {
        assert!(always_runs(&CloseCause::HandedBack(why)));
    }
    assert!(always_runs(&CloseCause::Signal("SIGTERM")));
    for cause in [
        CloseCause::User,
        CloseCause::Compositor,
        CloseCause::NvimExited,
        CloseCause::LastTerminalExit(ModuleId::terminal()),
        CloseCause::GroupClosed,
    ] {
        assert!(!always_runs(&cause), "{cause:?} may wait for an answer");
    }

    /// Puts the close in one of its states.
    type Start = (&'static str, CloseState, fn(&Rig));
    let states: [Start; 6] = [
        ("idle", CloseState::Idle, |_| {}),
        ("awaiting the facts", CloseState::Quitting, |r| {
            r.driver.close_requested(WIN, CloseBy::User);
        }),
        ("asking about drafts", CloseState::Asking, |r| {
            r.driver.close_requested(WIN, CloseBy::User);
            r.facts(CloseAsks {
                blocking: false,
                drafts: 1,
                proposals: 0,
                ..CloseAsks::default()
            });
        }),
        ("asking whether to give nvim back", CloseState::Asking, |r| {
            r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
        }),
        ("asking about turns, nvim let go of", CloseState::Asking, |r| {
            r.running();
            r.driver.close_requested(WIN, CloseBy::User);
            r.facts(CloseAsks::default());
        }),
        ("tearing down", CloseState::Closed, |r| {
            r.driver.close_requested(WIN, CloseBy::User);
            r.facts(CloseAsks::default());
            r.detached();
            r.advance(Duration::ZERO);
        }),
    ];
    for (name, state, into) in states {
        for signal in [false, true] {
            let r = attached(two());
            into(&r);
            assert_eq!(
                r.driver.query(|c| c.window(WIN).map(|w| w.close.state)),
                Some(state),
                "{name}"
            );
            if signal {
                r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
            } else {
                r.hand_back(HandBackWhy::OtherUi);
            }
            assert_eq!(r.detaches(), 1, "{name}, signal {signal}: let go of, once");
            if signal {
                assert_eq!(r.closes(), 1, "{name}: a signal closes at once");
            }
            r.finish();
            assert_eq!(r.closes(), 1, "{name}, signal {signal}");
            assert_eq!(r.detaches(), 1, "{name}, signal {signal}");
            r.assert_nothing_ends_or_records(name);
        }
    }

    // While the close moves modules to the other window: a hand-back lets go at once, and the close goes on when
    // the moves do.
    let r = attached_with(two(), |w| w.close.state = CloseState::Moving);
    r.hand_back(HandBackWhy::TakeBack);
    assert_eq!(r.detaches(), 1);
    let r = attached_with(two(), |w| w.close.state = CloseState::Moving);
    r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    assert_eq!((r.detaches(), r.closes()), (1, 1));
}

/// A `n` to the turns' question after the nvim was let go of keeps the window for its turns, and its editor, which
/// holds no nvim any more, leaves the screen as an ended editor does, so nothing dead holds the keys.
#[test]
fn a_declined_turns_question_after_the_detach_retires_the_editor_in_place() {
    for answered_first in [false, true] {
        let r = attached(two());
        r.running();
        r.driver.close_requested(WIN, CloseBy::User);
        r.facts(CloseAsks::default());
        assert_eq!(r.detaches(), 1);
        if answered_first {
            r.detached();
        }
        assert_eq!(r.prompt(), crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0));
        r.answer("n");
        if !answered_first {
            r.detached();
        }
        r.advance(DETACHED_WITHIN);
        r.advance(Duration::ZERO);
        let case = format!("the detach answered before the `n`: {answered_first}");
        assert_eq!(r.prompt(), None, "{case}");
        assert_eq!(r.closes(), 0, "{case}: the window stays for its turns");
        assert_eq!(r.state(), CloseState::Idle, "{case}");
        assert_eq!(
            r.editor_shown(),
            (false, false),
            "{case}: the editor left the screen and the keys"
        );
        r.assert_nothing_ends_or_records(&case);

        // The next close asks only about the turns: the nvim is gone already.
        r.driver.close_requested(WIN, CloseBy::User);
        assert_eq!(
            r.prompt(),
            crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0),
            "{case}"
        );
        r.answer("y");
        r.advance(Duration::ZERO);
        assert_eq!((r.fact_asks(), r.detaches(), r.closes()), (1, 1, 1), "{case}");
        r.assert_nothing_ends_or_records(&case);
    }
}

/// `prefix x` on an editor whose nvim was let go of already has nothing to give back: it is the window's close.
#[test]
fn prefix_x_after_the_nvim_was_let_go_of_closes_without_the_give_back_question() {
    let r = attached_with(two(), |w| w.close.let_go = true);
    r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
    assert_ne!(r.prompt().as_deref(), Some(GIVE_BACK_PROMPT));
    r.advance(Duration::ZERO);
    assert_eq!((r.fact_asks(), r.detaches(), r.closes()), (0, 0, 1));
    r.assert_nothing_ends_or_records("prefix x after the let-go");
}

/// The editor reporting nvim's exit after it let go of it closes nothing and asks nothing: that nvim is the
/// terminal's again. While the close waits for the detach's answer, the exit is that answer.
#[test]
fn an_exit_after_the_detach_reopens_no_close() {
    let r = attached(two());
    r.running();
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks::default());
    r.answer("n");
    r.detached();
    r.exited();
    r.advance(DETACHED_WITHIN);
    r.advance(Duration::ZERO);
    assert_eq!(r.prompt(), None, "nothing asked again");
    assert_eq!((r.fact_asks(), r.detaches(), r.closes()), (1, 1, 0));

    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks::default());
    r.exited();
    r.advance(Duration::ZERO);
    assert_eq!(
        (r.detaches(), r.closes()),
        (1, 1),
        "the exit settles the wait for the detach"
    );
    r.assert_nothing_ends_or_records("an exit while the detach is awaited");
}

/// An answer about an earlier close's ask, coming after that close went on without it, is not taken for the
/// current close's: what it says may be stale.
#[test]
fn close_facts_answer_only_their_own_ask() {
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    let first = r.last_ask();
    r.advance(CLOSE_FACTS_WITHIN);
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    r.answer("n");
    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(r.fact_asks(), 2);
    assert_ne!(r.last_ask(), first, "each ask has its own generation");
    r.facts_for(first, CloseAsks::default());
    assert_eq!(
        (r.prompt(), r.detaches(), r.state()),
        (None, 0, CloseState::Quitting),
        "the earlier close's answer is ignored, and this close still waits for its own"
    );
    r.facts(CloseAsks {
        blocking: false,
        drafts: 1,
        proposals: 0,
        ..CloseAsks::default()
    });
    assert_eq!(r.prompt(), Some(unsaved_in_editor_prompt(1, 0)));
    assert_eq!(r.detaches(), 0);

    // With no answer of its own, the close's own bound still runs.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    let first = r.last_ask();
    r.advance(CLOSE_FACTS_WITHIN);
    r.answer("n");
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts_for(first, CloseAsks::default());
    r.advance(CLOSE_FACTS_WITHIN);
    assert_eq!(r.prompt().as_deref(), Some(BLOCKING_PROMPT));
    assert_eq!(r.detaches(), 0);
}

/// A `y` to the last module's question confirms that close only: when a question about the editor is then declined,
/// a later close asks about the turns running by then.
#[test]
fn a_declined_editor_question_withdraws_the_last_modules_confirmation() {
    for decline in ["drafts", "blocking"] {
        let r = attached(two());
        crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(ModuleId::editor()));
        r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
        assert!(r.prompt().is_some(), "{decline}: the last module's question");
        r.answer("y");
        assert_eq!(r.fact_asks(), 1, "{decline}");
        r.facts(CloseAsks {
            blocking: decline == "blocking",
            drafts: usize::from(decline == "drafts"),
            proposals: 0,
            ..CloseAsks::default()
        });
        assert!(r.prompt().is_some(), "{decline}: the question about the editor");
        r.answer("n");
        assert_eq!((r.prompt(), r.detaches()), (None, 0), "{decline}");

        r.running();
        r.driver.close_requested(WIN, CloseBy::User);
        r.facts(CloseAsks::default());
        assert_eq!(r.detaches(), 1, "{decline}");
        assert_eq!(
            r.prompt(),
            crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0),
            "{decline}: the turn started since is asked about"
        );
        r.advance(DETACHED_WITHIN);
        assert_eq!(r.closes(), 0, "{decline}");
    }
}

/// The window is not torn down while the editor is still letting go of the nvim: the teardown waits for the
/// detach's answer, at most [`DETACHED_WITHIN`].
#[test]
fn the_teardown_waits_for_the_detach_to_be_answered() {
    assert!(DETACHED_WITHIN >= DETACH_TEARDOWN + DETACH_ROOM_WAIT);
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks::default());
    assert_eq!(r.detaches(), 1);
    r.advance(Duration::ZERO);
    r.advance(DETACHED_WITHIN - Duration::from_millis(1));
    assert_eq!(r.closes(), 0, "not torn down before the detach is answered");
    assert!(
        r.driver
            .query(|c| c.window(WIN).map(|w| w.close.is_ending(&w.quit)).unwrap_or_default()),
        "the window is on its way out meanwhile"
    );
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);

    // With no answer, the bound tears it down.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks::default());
    r.advance(DETACHED_WITHIN - Duration::from_millis(1));
    assert_eq!(r.closes(), 0);
    r.advance(Duration::from_millis(1));
    r.advance(Duration::ZERO);
    assert_eq!((r.detaches(), r.closes()), (1, 1));

    // A hand-back's close waits the same way.
    let r = attached(two());
    r.hand_back(HandBackWhy::OtherUi);
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 0);
    r.detached();
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
}

#[test]
fn a_close_with_no_way_back_names_the_remote_ui_command_and_asks_nothing_about_a_waiting_nvim() {
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks {
        blocking: true,
        no_way_back: Some("/run/nvim-n".into()),
        ..CloseAsks::default()
    });
    assert_eq!(
        r.prompt().as_deref(),
        Some("closing leaves nvim running with no window. To see it again: nvim --remote-ui --server /run/nvim-n. Close? (y/n)")
    );
    assert_eq!(r.detaches(), 0, "nothing is let go of while the question is up");
    r.answer("y");
    assert_eq!(r.detaches(), 1);
    assert_eq!(
        r.prompt(),
        None,
        "with no terminal to go back to, a waiting nvim is not asked about"
    );

    // The drafts come first, then the same question.
    let r = attached(two());
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks {
        drafts: 1,
        no_way_back: Some("/run/nvim-n".into()),
        ..CloseAsks::default()
    });
    assert_eq!(r.prompt(), Some(unsaved_in_editor_prompt(1, 0)));
    r.answer("y");
    assert_eq!(r.prompt(), Some(crate::wm::feedback::no_way_back_prompt("/run/nvim-n")));
    r.answer("n");
    assert_eq!(r.detaches(), 0, "a `n` keeps the nvim in the window");
}

#[test]
fn connect_typed_in_the_window_hands_back_without_a_question() {
    let r = attached(two());
    r.running();
    r.hand_back(HandBackWhy::Connected);
    assert_eq!(r.detaches(), 1);
}

/// The editor's own nvim moved its UI to another server (`:connect`), which the editor followed: from then on the
/// window shows an nvim it did not start, and every rule of one holds. Its close asks, lets go of it, and never quits,
/// ends, restarts or shuts it down.
#[test]
fn a_followed_connect_makes_the_editor_attached_and_its_close_detaches() {
    let r = owned(two());
    let since = r.ops().len();
    r.driver.post(HostEvent::Editor(EditorEvent::Followed {
        win: WIN,
        kind: EditorKind::Attached,
        left: None,
    }));
    assert!(
        r.driver.query(|c| c.window(WIN).is_some_and(|w| w.editor_attached())),
        "the window still treats the server it moved to as its own"
    );
    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(r.fact_asks(), 1, "the close did not ask the editor first");
    r.facts(CloseAsks::default());
    r.finish();
    assert_eq!(r.detaches(), 1, "the close did not let go of the server");
    let wrong: Vec<_> = r
        .ops_since(since)
        .into_iter()
        .filter_map(|op| match op {
            HostOp::Editor(call) if ends_or_records(&call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(
        wrong,
        Vec::<EditorCall>::new(),
        "a followed server got a quit, an end or a record's call"
    );
    assert_eq!(r.closes(), 1);
}

/// After a followed `:connect` the editor's own nvim runs on with no window; the close says where, with the command
/// that shows it again, whether the editor answers in time or not.
#[test]
fn the_close_after_a_connect_names_the_left_server() {
    let left = "/run/nvim-own";
    let followed = |r: &Rig| {
        r.driver.post(HostEvent::Editor(EditorEvent::Followed {
            win: WIN,
            kind: EditorKind::Attached,
            left: Some(left.into()),
        }))
    };

    // No answer in time: the question still names it.
    let r = owned(two());
    followed(&r);
    r.driver.close_requested(WIN, CloseBy::User);
    r.advance(CLOSE_FACTS_WITHIN);
    assert_eq!(r.prompt(), Some(crate::wm::feedback::no_way_back_prompt(left)));
    assert_eq!(r.detaches(), 0, "nothing is let go of while the question is up");
    r.answer("y");
    assert_eq!(r.detaches(), 1);

    // The editor's answer says where it still runs.
    let r = owned(two());
    followed(&r);
    r.driver.close_requested(WIN, CloseBy::User);
    r.facts(CloseAsks {
        no_way_back: Some(left.into()),
        ..CloseAsks::default()
    });
    assert_eq!(r.prompt(), Some(crate::wm::feedback::no_way_back_prompt(left)));
    r.answer("n");
    assert_eq!(r.detaches(), 0, "a `n` keeps the window");
}

/// A followed `:restart` is still the window's own nvim: its close quits it as before. An attached editor stays
/// attached whatever is reported.
#[test]
fn a_followed_restart_keeps_the_editor_owned() {
    let r = owned(two());
    r.driver.post(HostEvent::Editor(EditorEvent::Followed {
        win: WIN,
        kind: EditorKind::Owned,
        left: None,
    }));
    assert!(r.driver.query(|c| c.window(WIN).is_some_and(|w| !w.editor_attached())));

    // Nothing makes an nvim the window did not start its own.
    let a = attached(two());
    a.driver.post(HostEvent::Editor(EditorEvent::Followed {
        win: WIN,
        kind: EditorKind::Owned,
        left: None,
    }));
    assert!(
        a.driver.query(|c| c.window(WIN).is_some_and(|w| w.editor_attached())),
        "a followed restart made an attached editor the window's own"
    );

    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(
        r.fact_asks(),
        0,
        "an owned editor's close asked for an attached one's facts"
    );
}

/// The window's own nvim after a followed `:connect` to `left` (`None`: the `:connect` left none running).
fn connected(left: Option<&str>) -> Rig {
    let r = owned(two());
    r.driver.post(HostEvent::Editor(EditorEvent::Followed {
        win: WIN,
        kind: EditorKind::Attached,
        left: left.map(str::to_owned),
    }));
    r
}

/// The server a `:connect` reached ends (`:qa` typed there, a further `:connect`): the window's own nvim it left still
/// runs with no window, so the close names it before anything goes, and a `n` keeps the window. A later close asks
/// again; one that left nothing running closes as before.
#[test]
fn the_reached_servers_end_names_the_left_nvim_before_the_window_closes() {
    let left = "/run/nvim-own";
    let r = connected(Some(left));
    r.exited();
    assert_eq!(r.prompt(), Some(crate::wm::feedback::no_way_back_prompt(left)));
    assert_eq!(r.closes(), 0, "the window closed before it named the left nvim");
    r.answer("y");
    r.finish();
    assert_eq!(r.closes(), 1);
    assert_eq!(
        (r.fact_asks(), r.detaches()),
        (0, 0),
        "an ended server has nothing to ask or let go of"
    );
    r.assert_nothing_ends_or_records("the reached server's end");

    let r = connected(Some(left));
    r.exited();
    r.answer("n");
    r.advance(Duration::ZERO);
    assert_eq!((r.prompt(), r.closes()), (None, 0), "a `n` keeps the window");
    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(
        r.prompt(),
        Some(crate::wm::feedback::no_way_back_prompt(left)),
        "the next close does not name the left nvim"
    );
    r.answer("y");
    r.finish();
    assert_eq!(r.closes(), 1);
    r.assert_nothing_ends_or_records("a close after a declined question");

    let r = connected(None);
    r.exited();
    assert_eq!(
        r.prompt(),
        None,
        "a `:connect` that left nothing running is asked about"
    );
    r.advance(Duration::ZERO);
    assert_eq!(r.closes(), 1);
}

/// A close that cannot ask (a signal) still writes where the window's own nvim runs on, with the command that shows it
/// again.
#[test]
fn a_signal_close_after_a_connect_logs_the_left_nvim() {
    let left = "/run/nvim-own";
    let r = connected(Some(left));
    r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    r.finish();
    let notes = r.driver.query(|c| c.notes(WIN));
    assert!(
        notes
            .iter()
            .any(|note| note.contains(&format!("nvim --remote-ui --server {left}"))),
        "{notes:#?}"
    );
}

/// `prefix x` on an editor that followed a `:connect` has no terminal to give the nvim back to: its question says the
/// window closes, and its `y` is the window's close.
#[test]
fn prefix_x_after_a_connect_says_the_window_closes() {
    let r = connected(Some("/run/nvim-own"));
    r.driver.run(WIN, Action::ModuleKill, Origin::Key { keycode: 7 });
    assert_eq!(r.prompt().as_deref(), Some(crate::wm::feedback::CONNECTED_CLOSE_PROMPT));
    r.answer("y");
    assert_eq!(r.fact_asks(), 1, "the `y` is a close: it asks the editor first");
}
