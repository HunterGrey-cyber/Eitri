//! A tab handed between chat windows, whole: window 1's real pair and registry, each chat window's real link handlers
//! at the far end of a real socket pair, and a holder that stands for its panel. What the panel does when asked to
//! release or adopt has its own tests in the panel.

use std::cell::{Cell, RefCell};

use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::saved_tabs::{SavedTab, SavedTabs};
use eitri_core::tab_set::AgentId;

use super::*;
use crate::linked::hand_over::{answer_message, serve, HandOvers, TabHolder, TabHolderSlot};
use eitri_panel::agent_panel::TabAnswer;

/// What every window did, in one order.
type Log = Rc<RefCell<Vec<String>>>;

fn saved(session: &str) -> SavedTab {
    SavedTab {
        agent: AgentId::Claude,
        conversation_id: "0123456789abcdef".into(),
        provider_session_id: session.into(),
        name: Some("work".into()),
        mode: SessionModeChoice::Auto,
        view: None,
        window: None,
        turn_running: false,
        draft: None,
        row_key: None,
        plan_origin: None,
    }
}

/// A chat window's tabs, standing for its panel.
struct Tabs {
    name: String,
    link: LinkDriver,
    log: Log,
    /// Why a release keeps the tab, when it does.
    keeps: RefCell<Option<String>>,
    /// A release that has not let go yet: the session its answer waits for the test to give.
    holds_release: Cell<bool>,
    /// An adoption that does not settle: the window will end with the resume still connecting.
    holds_adoption: Cell<bool>,
    /// Why this window cannot take a tab up, when it cannot.
    refuses: RefCell<Option<String>>,
}

impl Tabs {
    fn say(&self, what: String) {
        self.log.borrow_mut().push(format!("{}: {what}", self.name));
    }

    fn answer(&self, answer: TabAnswer) {
        self.link.send(answer_message(answer), Vec::new()).unwrap();
    }

    fn let_go(&self, session: &str) {
        self.say(format!("let go {session}"));
        self.answer(TabAnswer::Released {
            session: session.to_string(),
            tab: saved(session),
        });
    }
}

impl TabHolder for Tabs {
    fn release(&self, _: &AgentId, session: &str) {
        if let Some(why) = self.keeps.borrow().clone() {
            self.say(format!("keeps {session}"));
            let session = session.to_string();
            self.answer(TabAnswer::Kept { session, why });
        } else if self.holds_release.get() {
            self.say(format!("asked to let go {session}"));
        } else {
            self.let_go(session);
        }
    }

    fn adopt(&self, tab: SavedTab) {
        let session = tab.provider_session_id;
        self.say(format!("resumes {session}"));
        if let Some(why) = self.refuses.borrow().clone() {
            self.answer(TabAnswer::NotAdopted { session, why });
        } else if !self.holds_adoption.get() {
            self.answer(TabAnswer::Adopted { session });
        }
    }

    fn kept(&self, why: String) {
        self.say(format!("told: {why}"));
    }

    fn left(&self) {
        self.say("its tab left".to_string());
    }

    fn given(&self, tabs: SavedTabs) {
        self.say(format!("given {} tab(s)", tabs.tabs.len()));
    }

    fn tabs_now(&self) -> Option<SavedTabs> {
        None
    }
}

/// The far end of one module window's link.
struct ChatWindow {
    peer: PeerId,
    link: LinkDriver,
    tabs: Rc<Tabs>,
}

fn chat_window(name: &WindowName, peer: PeerId, their: Their, log: &Log) -> ChatWindow {
    let link = LinkDriver::new(their.handle, their.inbound);
    link.set_linked();
    let tabs = Rc::new(Tabs {
        name: name.text(),
        link: link.clone(),
        log: log.clone(),
        keeps: RefCell::default(),
        holds_release: Cell::new(false),
        holds_adoption: Cell::new(false),
        refuses: RefCell::default(),
    });
    let slot: TabHolderSlot = Rc::default();
    *slot.borrow_mut() = Some(tabs.clone() as Rc<dyn TabHolder>);
    serve(&link, slot);
    ChatWindow { peer, link, tabs }
}

impl Group {
    /// A chat window that is up and linked.
    fn chat_window(&self, name: &WindowName, log: &Log) -> ChatWindow {
        let (peer, their, _) = self.link(name);
        chat_window(name, peer, their, log)
    }

    /// The window a hand-over started: takes its end of the link and says its hello.
    fn started_window(&self, name: &WindowName, log: &Log) -> ChatWindow {
        self.processes
            .borrow_mut()
            .push(self.pair.module_process(name).unwrap());
        let end = self.window_ends.borrow_mut().pop().expect("a link was made for it");
        let (handle, inbound) = LinkHandle::start(end, PresenceFlag::default()).unwrap();
        let their = Their { handle, inbound };
        self.say_hello(&their, name, name);
        let link = self.pair.module_link(name).unwrap();
        assert!(pump_until(&link, || link.linked()));
        let _ = welcome_fds(&their.inbound);
        chat_window(name, self.pair.module_peer(name).unwrap(), their, log)
    }

    fn hand_over(&self, from: PeerId, session: &str, to: &WindowName, token: Option<&str>) {
        let token = token.map(str::to_string);
        LinkedWindows::hand_over(&*self.pair, WinId::MAIN, from, &AgentId::Claude, session, to, token).unwrap();
    }

    /// Pumps every link until `done`, or five seconds.
    fn until(&self, windows: &[&ChatWindow], done: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            for name in self.pair.module_names() {
                if let Some(link) = self.pair.module_link(&name) {
                    link.pump();
                }
            }
            for window in windows {
                window.link.pump();
            }
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// Pumps every link for a moment, for what must not happen.
    fn quiet(&self, windows: &[&ChatWindow]) {
        for _ in 0..20 {
            for window in windows {
                for name in self.pair.module_names() {
                    if self.pair.module_peer(&name) != Some(window.peer) {
                        continue;
                    }
                    let Some(link) = self.pair.module_link(&name) else {
                        continue;
                    };
                    link.read_through_for_test(&window.link);
                    window.link.read_through_for_test(&link);
                    link.pump();
                }
                window.link.pump();
            }
        }
    }
}

fn has(log: &Log, line: &str) -> bool {
    log.borrow().iter().any(|said| said == line)
}

#[test]
fn tab_break_in_c1_hands_the_tab_to_a_new_chat_window() {
    let group = group("tab-break-new", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    group.hand_over(c1.peer, "s1", &chat(2), Some("their-token"));
    assert!(
        group.linked.borrow().iter().all(|(name, _)| *name != chat(2)),
        "the target is still starting"
    );
    assert!(log.borrow().is_empty(), "nothing lets go before the target has linked");

    let c2 = group.started_window(&chat(2), &log);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: its tab left")));
    assert_eq!(
        *log.borrow(),
        ["c1: let go s1", "c2: resumes s1", "c1: its tab left"].map(String::from)
    );
    let started = std::fs::read_to_string(&group.report).unwrap();
    assert!(started.contains("their-token"), "{started}");
}

#[test]
fn tab_break_in_c3_hands_the_tab_back_to_c1_and_c3_closes_when_empty() {
    let group = group("tab-break-back", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c3 = group.chat_window(&chat(3), &log);
    group.hand_over(c3.peer, "s1", &chat(1), None);
    assert!(group.until(&[&c1, &c3], || has(&log, "c3: its tab left")));
    assert_eq!(
        *log.borrow(),
        ["c3: let go s1", "c1: resumes s1", "c3: its tab left"].map(String::from),
        "c3 hears only once c1 holds the tab, so it stays open to take the tab back until then"
    );
    assert_eq!(
        std::fs::read_to_string(&group.report).unwrap().lines().count(),
        2,
        "c1 was open: nothing was started for it"
    );
}

#[test]
fn a_target_that_dies_before_adopting_gives_the_tab_back() {
    let group = group("tab-break-target-dies", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    c2.tabs.holds_adoption.set(true);
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c2: resumes s1")));
    let target = group.pair.module_link(&chat(2)).unwrap();
    c2.link.close("it crashed");
    assert!(group.until(&[&c1], || has(&log, "c1: resumes s1")));
    assert!(pump_until(&target, || !target.is_up()));
    assert!(!has(&log, "c1: its tab left"), "the tab is back where it was");
    c1.tabs.holds_release.set(true);
    let c3 = chat(3);
    group.hand_over(c1.peer, "s2", &c3, None);
    assert!(
        group.pair.module_peer(&c3).is_some(),
        "that hand-over is over, so another may begin"
    );
}

#[test]
fn a_target_that_ends_while_the_source_lets_go_gets_the_tab_back() {
    let group = group("tab-break-target-ends-early", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    c1.tabs.holds_release.set(true);
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: asked to let go s1")));
    c2.link.close("it crashed");
    assert!(group.until(&[&c1], || group.pair.module_link(&chat(2)).is_none()));
    c1.tabs.let_go("s1");
    assert!(group.until(&[&c1], || has(&log, "c1: resumes s1")));
}

#[test]
fn a_target_that_never_links_leaves_the_tab_where_it_is() {
    let group = group("tab-break-no-handshake", Duration::ZERO);
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    group.hand_over(c1.peer, "s1", &chat(2), None);
    group
        .processes
        .borrow_mut()
        .push(group.pair.module_process(&chat(2)).unwrap());
    group.clock.advance(Duration::ZERO);
    assert!(group.until(&[&c1], || has(&log, "c1: told: window c2 did not come up")));
    assert_eq!(log.borrow().len(), 1, "it was never asked to let go");
}

#[test]
fn a_target_that_cannot_take_the_tab_up_gives_it_back() {
    let group = group("tab-break-target-refuses", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    *c2.tabs.refuses.borrow_mut() = Some("open in another window".into());
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: resumes s1")));
}

#[test]
fn a_turn_starting_before_release_keeps_the_tab_where_it_is() {
    let group = group("tab-break-kept", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    *c1.tabs.keeps.borrow_mut() = Some("a turn started".into());
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: keeps s1")));
    group.quiet(&[&c1, &c2]);
    assert_eq!(
        *log.borrow(),
        ["c1: keeps s1"].map(String::from),
        "c2 was given nothing"
    );

    *c1.tabs.keeps.borrow_mut() = None;
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(
        group.until(&[&c1, &c2], || has(&log, "c2: resumes s1")),
        "the hand-over ended there, so another may begin"
    );
}

#[test]
fn a_second_tab_break_while_one_moves_is_refused() {
    let group = group("tab-break-second", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    c1.tabs.holds_release.set(true);
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: asked to let go s1")));
    group.hand_over(c1.peer, "s2", &chat(3), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: told: a tab is already moving")));
    assert!(
        group.pair.module_peer(&chat(3)).is_none(),
        "no window was started for the refused one"
    );
    // Another window's tab is no part of that one.
    c2.tabs.holds_release.set(true);
    group.hand_over(c2.peer, "s3", &chat(1), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c2: asked to let go s3")));
}

#[test]
fn the_session_is_never_held_by_both_windows() {
    let group = group("tab-break-one-holder", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    let c2 = group.chat_window(&chat(2), &log);
    c1.tabs.holds_release.set(true);
    group.hand_over(c1.peer, "s1", &chat(2), None);
    assert!(group.until(&[&c1, &c2], || has(&log, "c1: asked to let go s1")));
    group.quiet(&[&c1, &c2]);
    assert!(
        !has(&log, "c2: resumes s1"),
        "the source still holds the session: the target is given nothing yet"
    );
    c1.tabs.let_go("s1");
    assert!(group.until(&[&c1, &c2], || has(&log, "c2: resumes s1")));
    let at = |line: &str| log.borrow().iter().position(|said| said == line).unwrap();
    assert!(at("c1: let go s1") < at("c2: resumes s1"));
}

#[test]
fn a_tab_a_window_already_holds_refuses_going_to_itself() {
    let group = group("tab-break-itself", Duration::from_secs(30));
    let log = Log::default();
    let c1 = group.chat_window(&chat(1), &log);
    group.hand_over(c1.peer, "s1", &chat(1), None);
    assert!(group.until(&[&c1], || has(&log, "c1: told: the tab is in that window already")));
}

#[test]
fn a_tab_released_with_nothing_waiting_goes_back_to_the_window_that_let_it_go() {
    let mut hand_overs = HandOvers::default();
    let tab = saved("s1").to_wire();
    let sends = hand_overs.heard(
        &chat(2),
        Msg::TabReleased {
            session: "s1".into(),
            tab: tab.clone(),
        },
    );
    assert_eq!(
        sends,
        vec![(
            chat(2),
            Msg::TabsGiven {
                tabs: serde_json::json!([tab]),
                adopt: true
            }
        )]
    );
}

#[test]
fn the_release_names_the_tabs_agent_as_session_ids_are_each_agents_own() {
    let mut hand_overs = HandOvers::default();
    let agent = AgentId::Acp("two".into());
    let sends = hand_overs.begin(chat(1), (agent.clone(), "s1".into()), chat(2), true);
    assert_eq!(
        sends,
        vec![(
            chat(1),
            Msg::TabRelease {
                agent,
                session: "s1".into(),
                to: "c2".into()
            }
        )]
    );
}

#[test]
fn a_tab_let_go_on_a_link_whose_end_has_not_reached_this_thread_comes_back() {
    let dir = Scratch::new("hand-over-closed-link");
    let root = Path::new("/p");
    let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(PairEnv::for_tests(), two, chat(2), root).unwrap();
    let (handle, _inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    let log = Log::default();
    let (mine, _their) = fdpass::seqpacket_pair().unwrap();
    let (unused_handle, unused_inbound) = LinkHandle::start(mine, PresenceFlag::default()).unwrap();
    pair.set_tab_holder(Rc::new(Tabs {
        name: "c2".into(),
        link: LinkDriver::new(unused_handle, unused_inbound),
        log: log.clone(),
        keeps: RefCell::default(),
        holds_release: Cell::new(false),
        holds_adoption: Cell::new(false),
        refuses: RefCell::default(),
    }));
    handle
        .send(
            crate::linked::spawn::welcome(Default::default(), Vec::new()),
            vec![lock.share().unwrap()],
        )
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));
    drop(handle);
    // Window 1 is gone and its link's thread has stopped; nothing here has pumped that yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let written = link.send_confirmed(Msg::WindowFocused {}, Vec::new()).unwrap();
        if matches!(written.try_recv(), Ok(Err(_))) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the link's thread never stopped");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(link.linked());
    pair.answer_hand_over(TabAnswer::Released {
        session: "s1".into(),
        tab: saved("s1"),
    });
    assert!(has(&log, "c2: resumes s1"));
}

#[test]
fn a_tab_let_go_is_held_until_its_release_is_known_to_be_on_the_socket() {
    use std::sync::mpsc;
    let unwritten = crate::linked::hand_over::UnwrittenTabs::default();
    let (written, write_answer) = mpsc::channel();
    unwritten.hold(write_answer, saved("written"));
    let (failed, failed_answer) = mpsc::channel();
    unwritten.hold(failed_answer, saved("failed"));
    let (stopped, stopped_answer) = mpsc::channel::<std::io::Result<()>>();
    unwritten.hold(stopped_answer, saved("stopped"));
    assert!(unwritten.settle().is_empty(), "no answer yet: still held");

    written.send(Ok(())).unwrap();
    failed
        .send(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        .unwrap();
    drop(stopped);
    let lost: Vec<String> = unwritten
        .settle()
        .into_iter()
        .map(|tab| tab.provider_session_id)
        .collect();
    assert_eq!(lost, ["failed", "stopped"]);
    assert!(unwritten.settle().is_empty(), "each is answered once");
}

#[test]
fn a_release_that_was_queued_and_never_written_gives_the_tab_back_on_the_links_wake() {
    let dir = Scratch::new("hand-over-unwritten");
    let root = Path::new("/p");
    let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(PairEnv::for_tests(), two, chat(2), root).unwrap();
    let (handle, _inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    let log = Log::default();
    let (mine, _their) = fdpass::seqpacket_pair().unwrap();
    let (unused_handle, unused_inbound) = LinkHandle::start(mine, PresenceFlag::default()).unwrap();
    pair.set_tab_holder(Rc::new(Tabs {
        name: "c2".into(),
        link: LinkDriver::new(unused_handle, unused_inbound),
        log: log.clone(),
        keeps: RefCell::default(),
        holds_release: Cell::new(false),
        holds_adoption: Cell::new(false),
        refuses: RefCell::default(),
    }));
    handle
        .send(
            crate::linked::spawn::welcome(Default::default(), Vec::new()),
            vec![lock.share().unwrap()],
        )
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));

    let (answer, written) = std::sync::mpsc::channel();
    pair.hold_released(written, saved("s1"));
    link.pump();
    assert!(
        !has(&log, "c2: resumes s1"),
        "no answer yet: the tab is still on its way"
    );
    answer
        .send(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        .unwrap();
    link.pump();
    assert!(has(&log, "c2: resumes s1"));
}

#[test]
fn a_chat_windows_answers_reach_window_1_and_what_stays_is_said_there() {
    let dir = Scratch::new("hand-over-answers");
    let root = Path::new("/p");
    let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(PairEnv::for_tests(), two, chat(2), root).unwrap();
    let (handle, inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    assert!(matches!(
        inbound.recv_timeout(Duration::from_secs(5)).unwrap(),
        Inbound::Msg {
            msg: Msg::Hello { .. },
            ..
        }
    ));
    let log = Log::default();
    let (mine, _their) = fdpass::seqpacket_pair().unwrap();
    let (unused_handle, unused_inbound) = LinkHandle::start(mine, PresenceFlag::default()).unwrap();
    let tabs = Rc::new(Tabs {
        name: "c2".into(),
        link: LinkDriver::new(unused_handle, unused_inbound),
        log: log.clone(),
        keeps: RefCell::default(),
        holds_release: Cell::new(false),
        holds_adoption: Cell::new(false),
        refuses: RefCell::default(),
    });
    pair.set_tab_holder(tabs);

    // Window 1 is not linked yet: a tab let go comes straight back, as nobody else will be given it.
    pair.answer_hand_over(TabAnswer::Released {
        session: "s0".into(),
        tab: saved("s0"),
    });
    assert!(has(&log, "c2: resumes s0"));

    handle
        .send(
            crate::linked::spawn::welcome(Default::default(), Vec::new()),
            vec![lock.share().unwrap()],
        )
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));
    pair.answer_hand_over(TabAnswer::Kept {
        session: "s1".into(),
        why: "a turn started".into(),
    });
    assert!(has(&log, "c2: told: a turn started"));
    pair.answer_hand_over(TabAnswer::Released {
        session: "s2".into(),
        tab: saved("s2"),
    });
    pair.answer_hand_over(TabAnswer::Adopted { session: "s3".into() });
    pair.answer_hand_over(TabAnswer::NotAdopted {
        session: "s4".into(),
        why: "open in another window".into(),
    });
    assert!(has(&log, "c2: told: open in another window"));
    let mut heard = Vec::new();
    while heard.len() < 4 {
        match next_that(&inbound, Duration::from_secs(5), |msg| {
            matches!(
                msg,
                Msg::TabKept { .. } | Msg::TabReleased { .. } | Msg::TabAdopted { .. }
            )
            .then_some(msg)
        }) {
            Some(msg) => heard.push(msg),
            None => panic!("only {heard:?} came"),
        }
    }
    assert_eq!(
        heard,
        vec![
            Msg::TabKept {
                session: "s1".into(),
                why: "a turn started".into()
            },
            Msg::TabReleased {
                session: "s2".into(),
                tab: saved("s2").to_wire()
            },
            Msg::TabAdopted { session: "s3".into() },
            Msg::TabKept {
                session: "s4".into(),
                why: "open in another window".into()
            },
        ]
    );
}
