//! The group's tabs across real links: window 1's real pair and registry on one end, and on the other a test speaking
//! for the chat window, or a chat window's real pair with a holder that stands for its panel. Reading the file, merging
//! it and what the panel does with tabs given have their own tests.

use std::cell::Cell;
use std::time::Instant;

use eitri_core::agent_backend::AgentId;
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::saved_tabs::{load, Loaded, SavedTab, SavedTabs};
use serde_json::Value;

use super::*;
use crate::linked_window::group_tabs::{GroupTabs, REPORT_AFTER};
use crate::linked_window::hand_over::TabHolder;
use crate::linked_window::spawn::welcome;
use crate::linked_window::KEPT_WORDS;

fn c(n: u8) -> WindowName {
    WindowName::Chat(n)
}

fn tabs(sessions: &[&str]) -> SavedTabs {
    let tabs = sessions
        .iter()
        .map(|session| SavedTab {
            agent: AgentId::Claude,
            conversation_id: "0123456789abcdef".into(),
            provider_session_id: session.to_string(),
            name: None,
            mode: SessionModeChoice::Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
        })
        .collect();
    SavedTabs::from_parts(tabs, 0, Vec::new(), None)
}

/// A group whose window 1 keeps its tabs under the group's scratch directory.
fn keeping(label: &str) -> Group {
    let group = group(label, Duration::from_secs(30));
    group
        .pair
        .keep_group_tabs(GroupTabs::read(Some(&group.dir.0), &group.dir.1, false));
    group
}

fn sessions_on_disk(group: &Group) -> Vec<String> {
    match load(&group.dir.0, &group.dir.1) {
        Loaded::Saved(saved) => saved.tabs.into_iter().map(|t| t.provider_session_id).collect(),
        Loaded::Missing => Vec::new(),
        Loaded::Unusable(why) => panic!("{why}"),
    }
}

/// What `their` window is given next, if anything comes within `within`.
fn given_within(their: &Their, within: Duration) -> Option<(Value, bool)> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match their.inbound.recv_timeout(left) {
            Ok(Inbound::Msg {
                msg: Msg::TabsGiven { tabs, adopt },
                ..
            }) => return Some((tabs, adopt)),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

#[test]
fn a_respawned_chat_window_gets_its_tabs_again() {
    let group = keeping("tabs-respawn");
    let (_, their, _fds) = group.link(&c(2));
    let link = group.pair.module_link(&c(2)).unwrap();
    assert_eq!(
        given_within(&their, Duration::from_millis(100)),
        None,
        "a window nothing is kept for is given nothing"
    );
    their
        .handle
        .send(
            Msg::TabsReport {
                tabs: tabs(&["s-1", "s-2"]).to_wire(),
            },
            Vec::new(),
        )
        .unwrap();
    assert!(pump_until(&link, || sessions_on_disk(&group) == ["s-1", "s-2"]));

    // It dies: the link ends with no goodbye, and its tabs stay.
    drop(their);
    assert!(pump_until(&link, || link.ended() == Some(LinkEnd::Eof)));
    assert_eq!(group.pair.module_names(), Vec::<WindowName>::new());
    assert_eq!(sessions_on_disk(&group), ["s-1", "s-2"], "its sessions are still saved");

    // Its key starts it again, and at its link it has them back, restored as a launch restores them.
    let (_, their, _fds) = group.link(&c(2));
    let (given, adopt) = given_within(&their, Duration::from_secs(5)).expect("its tabs");
    assert!(!adopt);
    let given = SavedTabs::from_wire(&given).unwrap();
    assert_eq!(given.tabs.len(), 2);
    assert_eq!(given.active, Some(0));
}

#[test]
fn a_clean_close_of_a_linked_window_takes_its_tabs_out_of_the_file() {
    let group = keeping("tabs-bye");
    let (_, one, _fds) = group.link(&c(1));
    let (_, two, _fds) = group.link(&c(2));
    let (first, second) = (
        group.pair.module_link(&c(1)).unwrap(),
        group.pair.module_link(&c(2)).unwrap(),
    );
    for (their, session) in [(&one, "s-1"), (&two, "s-2")] {
        let report = Msg::TabsReport {
            tabs: tabs(&[session]).to_wire(),
        };
        their.handle.send(report, Vec::new()).unwrap();
    }
    assert!(pump_until(&first, || {
        second.pump();
        sessions_on_disk(&group) == ["s-1", "s-2"]
    }));

    two.handle.close("window 2 closed");
    assert!(pump_until(&second, || second.ended().is_some()));
    assert_eq!(
        sessions_on_disk(&group),
        ["s-1", "s-2"],
        "too early to tell whose close it was"
    );
    group.pair.settle_closes_at(past_the_burst());
    assert!(pump_until(&first, || sessions_on_disk(&group) == ["s-1"]));
    let (_, again, _fds) = group.link(&c(2));
    assert_eq!(
        given_within(&again, Duration::from_millis(100)),
        None,
        "tabs the user closed are not reopened"
    );
}

/// When the burst's end has come for everything heard so far.
fn past_the_burst() -> Instant {
    Instant::now() + eitri_core::window_link::close_burst::CLOSE_BURST
}

/// Two linked chat windows with a tab each, on disk.
fn two_windows(label: &str) -> (Group, [Their; 2], [LinkDriver; 2], Vec<OwnedFd>) {
    let group = keeping(label);
    let (_, one, mut fds) = group.link(&c(1));
    let (_, two, more) = group.link(&c(2));
    fds.extend(more);
    let (first, second) = (
        group.pair.module_link(&c(1)).unwrap(),
        group.pair.module_link(&c(2)).unwrap(),
    );
    for (their, session) in [(&one, "s-1"), (&two, "s-2")] {
        let report = Msg::TabsReport {
            tabs: tabs(&[session]).to_wire(),
        };
        their.handle.send(report, Vec::new()).unwrap();
    }
    assert!(pump_until(&first, || {
        second.pump();
        sessions_on_disk(&group) == ["s-1", "s-2"]
    }));
    (group, [one, two], [first, second], fds)
}

/// A logout closes window 1 with the rest, and which of them hears it first depends on the compositor's list: however
/// they fall, every window's tabs stay.
#[test]
fn windows_closed_as_window_1_is_asked_to_close_stay_in_the_file() {
    for window_1_first in [true, false] {
        let (group, [one, two], [first, second], _fds) = two_windows("tabs-logout");
        if window_1_first {
            group.pair.group_asked();
        }
        one.handle.close("window c1 closed");
        two.handle.close("window c2 closed");
        assert!(pump_until(&first, || first.ended().is_some()));
        assert!(pump_until(&second, || second.ended().is_some()));
        if !window_1_first {
            group.pair.group_asked();
        }
        group.pair.settle_closes_at(past_the_burst());
        assert_eq!(sessions_on_disk(&group), ["s-1", "s-2"]);
        let (_, again, _fds) = group.link(&c(2));
        assert!(
            given_within(&again, Duration::from_secs(5)).is_some(),
            "its tabs wait for it, and it may be opened again if the logout is called off"
        );
    }
}

/// Window 1 left alone, windows closed in quick succession are closed all the same.
#[test]
fn windows_closed_by_hand_in_quick_succession_are_all_closed() {
    let (group, [one, two], [first, second], _fds) = two_windows("tabs-by-hand");
    one.handle.close("window c1 closed");
    two.handle.close("window c2 closed");
    assert!(pump_until(&first, || first.ended().is_some()));
    assert!(pump_until(&second, || second.ended().is_some()));
    group.pair.settle_closes_at(past_the_burst());
    assert!(pump_until(&first, || sessions_on_disk(&group).is_empty()));
}

/// A window opened again before its close has settled is a new window, as it is after.
#[test]
fn a_window_reopened_inside_its_burst_starts_empty() {
    let (group, [_one, two], [first, second], _fds) = two_windows("tabs-reopened");
    two.handle.close("window c2 closed");
    assert!(pump_until(&second, || second.ended().is_some()));
    let (_, again, _fds) = group.link(&c(2));
    assert_eq!(given_within(&again, Duration::from_millis(100)), None);
    assert!(pump_until(&first, || sessions_on_disk(&group) == ["s-1"]));
}

/// The hook hears a window once its close is settled, and never one that stays.
#[test]
fn what_else_is_kept_for_a_window_goes_only_when_its_close_is_settled() {
    let heard: Rc<RefCell<Vec<WindowName>>> = Rc::default();
    let (group, [one, two], [first, second], _fds) = two_windows("tabs-forgotten");
    {
        let heard = heard.clone();
        group
            .pair
            .on_window_forgotten(Rc::new(move |name| heard.borrow_mut().push(name.clone())));
    }
    one.handle.close("window c1 closed");
    assert!(pump_until(&first, || first.ended().is_some()));
    assert!(heard.borrow().is_empty());
    group.pair.settle_closes_at(past_the_burst());
    assert_eq!(*heard.borrow(), [c(1)]);

    group.pair.group_asked();
    two.handle.close("window c2 closed");
    assert!(pump_until(&second, || second.ended().is_some()));
    group.pair.settle_closes_at(past_the_burst());
    assert_eq!(
        *heard.borrow(),
        [c(1)],
        "a window that went as window 1 was asked keeps its place"
    );
}

/// The wait is a timer: nobody has to ask for a close to take effect.
#[test]
fn a_close_settles_by_a_timer_of_its_own() {
    let (group, [_one, two], [_first, second], _fds) = two_windows("tabs-timer");
    two.handle.close("window c2 closed");
    assert!(pump_until(&second, || second.ended().is_some()));
    assert!(group.timers.borrow().len() >= 1, "the close armed the wait");
}

/// A window a signal ended, or one with tabs on offer, was not closed by the user: its tabs stay for its next start.
#[test]
fn a_window_kept_at_its_close_stays_in_the_file() {
    let group = keeping("tabs-kept");
    let (_, one, _fds) = group.link(&c(1));
    let (_, two, _fds) = group.link(&c(2));
    let (first, second) = (
        group.pair.module_link(&c(1)).unwrap(),
        group.pair.module_link(&c(2)).unwrap(),
    );
    for (their, session) in [(&one, "s-1"), (&two, "s-2")] {
        let report = Msg::TabsReport {
            tabs: tabs(&[session]).to_wire(),
        };
        their.handle.send(report, Vec::new()).unwrap();
    }
    assert!(pump_until(&first, || {
        second.pump();
        sessions_on_disk(&group) == ["s-1", "s-2"]
    }));

    two.handle.close(&format!("window c2 {KEPT_WORDS}"));
    assert!(pump_until(&second, || second.ended().is_some()));
    assert!(!second.ended().unwrap().closed_for_good());
    assert_eq!(group.pair.module_names(), [c(1)], "the window has gone");
    assert_eq!(sessions_on_disk(&group), ["s-1", "s-2"]);
    let (_, again, _fds) = group.link(&c(2));
    assert!(given_within(&again, Duration::from_secs(5)).is_some());
}

/// A whole window's tabs pass one packet with a few hundred sessions, both ways.
#[test]
fn a_window_of_hundreds_of_tabs_is_reported_and_given_back() {
    let group = keeping("tabs-many");
    let (_, their, _fds) = group.link(&c(1));
    let link = group.pair.module_link(&c(1)).unwrap();
    let names: Vec<String> = (0..600).map(|n| format!("019a0000-0000-7000-8000-{n:012}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let report = Msg::TabsReport {
        tabs: tabs(&names).to_wire(),
    };
    assert!(eitri_core::window_link::msg::to_json(&report).len() > eitri_core::window_link::INLINE_MAX);
    their.handle.send(report, Vec::new()).unwrap();
    assert!(pump_until(&link, || sessions_on_disk(&group).len() == 600));

    drop(their);
    assert!(pump_until(&link, || link.ended() == Some(LinkEnd::Eof)));
    let (_, their, _fds) = group.link(&c(1));
    let (given, _) = given_within(&their, Duration::from_secs(5)).expect("its tabs");
    assert_eq!(SavedTabs::from_wire(&given).unwrap().tabs.len(), 600);
}

/// A change can still be gathering in a chat window when window 1 closes. Window 1 asks first, with the link up, and its
/// `bye` and the file's last write wait for the answer.
#[test]
fn a_closing_group_hears_each_chat_windows_last_tabs_before_its_bye() {
    let group = keeping("tabs-last-call");
    let (_, their, _fds) = group.link(&c(2));
    let link = group.pair.module_link(&c(2)).unwrap();
    let report = |sessions: &[&str]| Msg::TabsReport {
        tabs: tabs(sessions).to_wire(),
    };
    their.handle.send(report(&["s-1"]), Vec::new()).unwrap();
    assert!(pump_until(&link, || sessions_on_disk(&group) == ["s-1"]));

    group.pair.end_group();
    let write = group.pair.finish_group_tabs().expect("the last write is still to come");
    let asked = next_that(&their.inbound, Duration::from_secs(5), |msg| {
        matches!(msg, Msg::GroupClosing {}).then_some(())
    });
    assert!(asked.is_some());
    group.fire_timers();
    assert_eq!(group.pair.module_names(), [c(2)], "no goodbye while its answer is owed");

    their.handle.send(report(&["s-1", "s-2"]), Vec::new()).unwrap();
    their.handle.send(Msg::TabsSaid {}, Vec::new()).unwrap();
    assert!(pump_until(&link, || {
        group.fire_timers();
        group.pair.module_names().is_empty()
    }));
    assert_eq!(heard_bye(&their).as_deref(), Some("window 1 closed"));
    assert!(write.recv_timeout(Duration::from_secs(5)).is_ok());
    assert_eq!(sessions_on_disk(&group), ["s-1", "s-2"]);
}

/// A window that stopped reading holds its link's close for the whole flush, and a group can have several.
#[test]
fn a_groups_links_close_off_the_gtk_thread() {
    let text = eitri_core::source_scan::read_required("shell/src/linked_window/registry.rs").text;
    let code = eitri_core::source_scan::code_only(&text);
    assert!(code.contains(concat!("module.link.close_", "detached(why)")));
}

#[test]
fn a_chat_window_that_does_not_answer_does_not_hold_the_close() {
    let group = keeping("tabs-last-silent");
    let (_, their, _fds) = group.link(&c(2));
    group.pair.end_group();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !group.pair.module_names().is_empty() {
        assert!(std::time::Instant::now() < deadline, "the wait is bounded");
        group.fire_timers();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(heard_bye(&their).as_deref(), Some("window 1 closed"));
}

/// With nobody to ask nothing waits: a page window has no tabs, and a group with `agent.restore` off keeps none.
#[test]
fn a_group_with_no_chat_to_hear_closes_at_once() {
    let pages = keeping("tabs-last-pages");
    let (_, page, _fds) = pages.link(&WindowName::Page("markdown".into()));
    pages.pair.end_group();
    assert!(pages.pair.module_names().is_empty());
    assert_eq!(heard_bye(&page).as_deref(), Some("window 1 closed"));

    let unkept = group("tabs-last-off", Duration::from_secs(30));
    let (_, chat, _fds) = unkept.link(&c(1));
    unkept.pair.end_group();
    assert!(unkept.pair.module_names().is_empty());
    assert_eq!(heard_bye(&chat).as_deref(), Some("window 1 closed"));
}

#[test]
fn a_window_that_never_linked_leaves_what_is_kept_for_it() {
    let group = keeping("tabs-unlinked");
    let (_, one, _fds) = group.link(&c(2));
    let link = group.pair.module_link(&c(2)).unwrap();
    let report = Msg::TabsReport {
        tabs: tabs(&["s-1"]).to_wire(),
    };
    one.handle.send(report, Vec::new()).unwrap();
    assert!(pump_until(&link, || sessions_on_disk(&group) == ["s-1"]));
    drop(one);
    assert!(pump_until(&link, || link.ended().is_some()));

    // The next start says goodbye in its handshake: it was refused, and closed no tab.
    let (_, their) = group.open(&c(2));
    let link = group.pair.module_link(&c(2)).unwrap();
    their.handle.close("another build");
    assert!(pump_until(&link, || link.ended().is_some()));
    assert_eq!(sessions_on_disk(&group), ["s-1"]);
    let (_, their, _fds) = group.link(&c(2));
    assert!(given_within(&their, Duration::from_secs(5)).is_some());
}

/// What a chat window asked to run later, held for the test to fire.
type Timers = Rc<RefCell<Vec<(Duration, Box<dyn FnOnce()>)>>>;

/// A chat window's end of a link: its real pair, linked to a test that speaks for window 1, its panel a stand-in and
/// its timers held for the test to fire.
struct ChatWindow {
    pair: Rc<Pair>,
    link: LinkDriver,
    window1: LinkHandle,
    inbound: Receiver<Inbound>,
    holder: Rc<Holder>,
    timers: Timers,
    _dir: Scratch,
    _lock: PairLock,
}

/// A chat window's panel, as far as its tabs go.
#[derive(Default)]
struct Holder {
    now: RefCell<Option<SavedTabs>>,
    unopened: RefCell<Option<SavedTabs>>,
    given: RefCell<Vec<SavedTabs>>,
    told_adopt: Cell<u32>,
}

impl TabHolder for Holder {
    fn release(&self, _: &AgentId, _: &str) {
        unreachable!("no hand-over here");
    }

    fn adopt(&self, _: SavedTab) {
        self.told_adopt.set(self.told_adopt.get() + 1);
    }

    fn kept(&self, _: String) {}

    fn left(&self) {}

    fn given(&self, tabs: SavedTabs) {
        self.given.borrow_mut().push(tabs);
    }

    fn tabs_now(&self) -> Option<SavedTabs> {
        self.now.borrow().clone()
    }

    fn unopened(&self) -> Option<SavedTabs> {
        self.unopened.borrow().clone()
    }
}

fn chat_window(label: &str) -> ChatWindow {
    let dir = Scratch::new(label);
    let lock = PairLock::try_take(&dir.0, Path::new("/p")).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(two, c(2), &dir.1).unwrap();
    let (window1, inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    match inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
        Inbound::Msg {
            msg: Msg::Hello { .. }, ..
        } => {}
        other => panic!("the window said {other:?} first"),
    }
    let timers: Timers = Rc::default();
    let after: After = {
        let timers = timers.clone();
        Rc::new(move |within, run| timers.borrow_mut().push((within, run)))
    };
    pair.set_starts(Starts {
        after,
        ..Starts::default()
    });
    let holder = Rc::new(Holder::default());
    pair.set_tab_holder(holder.clone());
    ChatWindow {
        link: pair.link().unwrap(),
        pair,
        window1,
        inbound,
        holder,
        timers,
        _dir: dir,
        _lock: lock,
    }
}

impl ChatWindow {
    fn link_up(&self) {
        let welcome = welcome(Default::default(), Vec::new());
        self.window1.send(welcome, vec![self._lock.share().unwrap()]).unwrap();
        assert!(pump_until(&self.link, || self.link.linked()));
    }

    fn fire_timers(&self) {
        let timers: Vec<_> = self.timers.borrow_mut().drain(..).collect();
        for (_, timer) in timers {
            timer();
        }
    }

    /// Each set window 1 hears within a moment, and whether it heard `bye` after them.
    fn reports(&self) -> (Vec<Vec<String>>, bool) {
        let mut heard = Vec::new();
        while let Ok(inbound) = self.inbound.recv_timeout(Duration::from_millis(200)) {
            match inbound {
                Inbound::Msg {
                    msg: Msg::TabsReport { tabs },
                    ..
                } => {
                    let set = SavedTabs::from_wire(&tabs).unwrap();
                    heard.push(set.tabs.into_iter().map(|t| t.provider_session_id).collect());
                }
                Inbound::Msg {
                    msg: Msg::Bye { .. }, ..
                } => return (heard, true),
                _ => {}
            }
        }
        (heard, false)
    }
}

#[test]
fn a_chat_window_an_end_signal_closes_says_it_is_kept() {
    let window = chat_window("tabs-signal");
    window.link_up();
    *window.holder.now.borrow_mut() = Some(tabs(&["s-1"]));
    assert!(LinkEnd::Bye {
        why: window.pair.closed_words()
    }
    .closed_for_good());
    window.pair.keep_this_window();
    let words = window.pair.closed_words();
    assert_eq!(words, format!("window c2 {KEPT_WORDS}"));
    assert!(!LinkEnd::Bye { why: words.clone() }.closed_for_good());
    window.link.close(&words);
    assert_eq!(window.reports(), (vec![vec!["s-1".to_string()]], true));
}

/// Closing the window answers no offer: what stays is the tabs on offer, not the ones the user had open and closed.
#[test]
fn a_chat_window_closed_with_tabs_on_offer_keeps_only_those() {
    let window = chat_window("tabs-offer");
    window.link_up();
    *window.holder.now.borrow_mut() = Some(tabs(&["opened"]));
    *window.holder.unopened.borrow_mut() = Some(tabs(&["offered"]));
    let words = window.pair.closed_words();
    assert!(!LinkEnd::Bye { why: words.clone() }.closed_for_good());
    window.link.close(&words);
    assert_eq!(window.reports(), (vec![vec!["offered".to_string()]], true));
}

#[test]
fn a_chat_window_asked_for_its_last_tabs_says_them_with_the_link_up() {
    let window = chat_window("tabs-asked");
    window.link_up();
    let asked = |window: &ChatWindow| {
        window.window1.send(Msg::GroupClosing {}, Vec::new()).unwrap();
        let mut heard = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline, "it answers");
            window.link.pump();
            match window.inbound.recv_timeout(Duration::from_millis(10)) {
                Ok(Inbound::Msg {
                    msg: Msg::TabsReport { tabs },
                    ..
                }) => heard.extend(
                    SavedTabs::from_wire(&tabs)
                        .unwrap()
                        .tabs
                        .into_iter()
                        .map(|t| t.provider_session_id),
                ),
                Ok(Inbound::Msg {
                    msg: Msg::TabsSaid {}, ..
                }) => return heard,
                _ => {}
            }
        }
    };
    assert_eq!(
        asked(&window),
        Vec::<String>::new(),
        "a window that holds nothing says nothing, and that it is done"
    );
    *window.holder.now.borrow_mut() = Some(tabs(&["s-1", "s-2"]));
    assert_eq!(asked(&window), ["s-1", "s-2"]);
    assert!(window.link.linked(), "the link stays for window 1 to close");
}

/// A change on its own is in window 1's hands as soon as it happens, so a close right after it loses nothing; only what
/// follows close behind waits on the timer.
#[test]
fn a_lone_tab_change_goes_out_at_once_and_what_follows_it_gathers() {
    let window = chat_window("tabs-burst");
    window.link_up();
    window.pair.tabs_changed(tabs(&["s-1"]));
    window.pair.tabs_changed(tabs(&["s-1", "s-2"]));
    window.pair.tabs_changed(tabs(&["s-2"]));
    assert_eq!(window.timers.borrow().len(), 1, "one timer gathers them");
    assert_eq!(window.timers.borrow()[0].0, REPORT_AFTER);
    assert_eq!(
        window.reports(),
        (vec![vec!["s-1".to_string()]], false),
        "the first, and none of the rest yet"
    );
    window.fire_timers();
    assert_eq!(window.reports().0, [["s-2".to_string()]]);

    window.pair.tabs_changed(tabs(&["s-3"]));
    assert_eq!(window.reports().0, [["s-3".to_string()]], "quiet again, so at once");
    assert_eq!(window.timers.borrow().len(), 1, "with a timer for what follows it");
    window.fire_timers();
    assert_eq!(window.reports().0, Vec::<Vec<String>>::new(), "and nothing did");
}

#[test]
fn a_change_with_no_link_up_goes_out_when_one_comes() {
    let window = chat_window("tabs-before-link");
    window.pair.tabs_changed(tabs(&["s-1"]));
    window.fire_timers();
    window.pair.say_tabs();
    assert_eq!(window.reports().0, Vec::<Vec<String>>::new());
    window.link_up();
    window.pair.say_tabs();
    assert_eq!(window.reports().0, [["s-1".to_string()]]);
}

/// The window's own close, as the teardown runs it. A report is gathered still, and the panel can say newer tabs yet.
#[test]
fn a_closing_chat_window_says_its_last_tabs_ahead_of_its_bye() {
    let window = chat_window("tabs-closing");
    window.link_up();
    window.pair.tabs_changed(tabs(&["s-0"]));
    window.pair.tabs_changed(tabs(&["s-1"]));
    *window.holder.now.borrow_mut() = Some(tabs(&["s-1", "s-2"]));
    window.link.close("window 2 closed");
    assert_eq!(
        window.reports(),
        (
            vec![vec!["s-0".to_string()], vec!["s-1".to_string(), "s-2".to_string()]],
            true
        ),
        "with no wait, ahead of the `bye`"
    );
}

#[test]
fn a_closing_chat_window_whose_panel_is_gone_says_what_it_saw_last() {
    let window = chat_window("tabs-closing-late");
    window.link_up();
    window.pair.tabs_changed(tabs(&["s-0"]));
    window.pair.tabs_changed(tabs(&["s-1"]));
    window.link.close("window 2 closed");
    assert_eq!(
        window.reports(),
        (vec![vec!["s-0".to_string()], vec!["s-1".to_string()]], true)
    );
}

#[test]
fn tabs_given_at_a_link_reach_the_panel_and_a_hand_overs_stay_its_own() {
    let window = chat_window("tabs-given");
    window.link_up();
    let kept = Msg::TabsGiven {
        tabs: tabs(&["s-1"]).to_wire(),
        adopt: false,
    };
    window.window1.send(kept, Vec::new()).unwrap();
    assert!(pump_until(&window.link, || window.holder.given.borrow().len() == 1));
    assert_eq!(window.holder.given.borrow()[0], tabs(&["s-1"]));

    let bad = Msg::TabsGiven {
        tabs: serde_json::json!([1]),
        adopt: false,
    };
    let handed = Msg::TabsGiven {
        tabs: Value::Array(vec![tabs(&["s-2"]).tabs[0].to_wire()]),
        adopt: true,
    };
    window.window1.send(bad, Vec::new()).unwrap();
    window.window1.send(handed, Vec::new()).unwrap();
    assert!(pump_until(&window.link, || window.holder.told_adopt.get() == 1));
    assert_eq!(
        window.holder.given.borrow().len(),
        1,
        "what is no set of tabs is dropped"
    );
}
