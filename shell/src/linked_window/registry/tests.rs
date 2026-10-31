use std::cell::RefCell;
use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eitri_core::window_link::fdpass;
use eitri_core::window_link::link::{Inbound, LinkHandle, PresenceFlag};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::pair::{pair_lock_name, PairLock};
use eitri_core::window_link::{FocusTarget, Msg};
use eitri_core::wm::{Controller, Driver, PeerId, WinId};

use super::{ModuleWindows, OpenModule, MODULE_WINDOWS_ARE_WINDOW_1S};
use crate::gtk_host::LinkedWindows;
use crate::linked_window::driver::tests::{controller, pump_until};
use crate::linked_window::spawn::{hello, After, LinkEnds, Pair, PairHooks, SecondProcess, Starts, HUB_PEER};
use crate::linked_window::tests::Scratch;
use crate::linked_window::{LinkDriver, LinkEnd};

/// A stand-in for a module window: it closes its copy of the link, so the test's copy of that end is the only one
/// and the test speaks for the window, then sleeps. Its arguments are written where `report` says.
fn stand_in(report: &Path) -> String {
    format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\nexec 3>&-\nexec sleep 30\n",
        report.display()
    )
}

/// The window's side of one link, in the test's hands.
struct Their {
    handle: LinkHandle,
    inbound: Receiver<Inbound>,
}

/// A window 1 holding the group lock, whose module windows are stand-ins. What its hooks were called with is
/// recorded, and the handshakes' timers are held for the test to fire.
struct Group {
    pair: Rc<Pair>,
    wm: Rc<Driver<Controller>>,
    window_ends: Rc<RefCell<Vec<OwnedFd>>>,
    timers: Rc<RefCell<Vec<Box<dyn FnOnce()>>>>,
    linked: Rc<RefCell<Vec<(WindowName, PeerId)>>>,
    ended: Rc<RefCell<Vec<(WindowName, PeerId)>>>,
    /// The windows reported as never linked, with why.
    failed: Rc<RefCell<Vec<(WindowName, String)>>>,
    processes: RefCell<Vec<Rc<SecondProcess>>>,
    report: PathBuf,
    dir: Scratch,
}

fn group(label: &str, within: Duration) -> Group {
    let dir = Scratch::new(label);
    let report = dir.1.join("started");
    let exe = dir.1.join("module-window-stand-in");
    std::fs::write(&exe, stand_in(&report)).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = PairLock::try_take(&dir.0, Path::new("/p")).unwrap().unwrap();
    let pair = Pair::full(Ok(Some(lock)), &dir.1, false, false);
    let timers: Rc<RefCell<Vec<Box<dyn FnOnce()>>>> = Rc::default();
    let after: After = {
        let timers = timers.clone();
        Rc::new(move |_, run| timers.borrow_mut().push(run))
    };
    let window_ends: Rc<RefCell<Vec<OwnedFd>>> = Rc::default();
    let link_ends: LinkEnds = {
        let window_ends = window_ends.clone();
        Rc::new(move || {
            let (mine, theirs) = fdpass::seqpacket_pair()?;
            window_ends.borrow_mut().push(theirs.try_clone()?);
            Ok((mine, theirs))
        })
    };
    pair.set_starts(Starts {
        exe,
        handshake_within: within,
        after,
        link_ends,
    });
    let (wm, _host) = controller();
    let linked: Rc<RefCell<Vec<(WindowName, PeerId)>>> = Rc::default();
    let ended: Rc<RefCell<Vec<(WindowName, PeerId)>>> = Rc::default();
    let failed: Rc<RefCell<Vec<(WindowName, String)>>> = Rc::default();
    {
        let (linked, ended) = (linked.clone(), ended.clone());
        let wm = Rc::downgrade(&wm);
        pair.set_hooks(PairHooks {
            linked: Rc::new(move |name, peer, link| {
                linked.borrow_mut().push((name.clone(), peer));
                link.attach_controller(WinId::MAIN, peer, name.clone(), &wm.upgrade().unwrap());
            }),
            post: Rc::new(|_| {}),
            ended: Rc::new(move |name, peer, _, _| ended.borrow_mut().push((name.clone(), peer))),
        });
        let failed = failed.clone();
        pair.on_module_failed(Rc::new(move |name, why| {
            failed.borrow_mut().push((name.clone(), why.to_string()))
        }));
    }
    Group {
        pair,
        wm,
        window_ends,
        timers,
        linked,
        ended,
        failed,
        processes: RefCell::default(),
        report,
        dir,
    }
}

impl Group {
    /// What the next window started runs after it has closed its copy of the link. Replaced, not written over: a stand-in
    /// that is running reads the script it came from. One still on its way to its script would read this one instead,
    /// so a test sees the last one running first.
    fn stand_ins_run(&self, body: &str) {
        let exe = self.dir.1.join("module-window-stand-in");
        let next = self.dir.1.join("next-stand-in");
        std::fs::write(&next, format!("#!/bin/sh\nexec 3>&-\n{body}\n")).unwrap();
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&next, &exe).unwrap();
    }

    /// `open_module`, tried again while the stand-in reads as busy: a process another test forks while the stand-in
    /// is being written keeps it open for writing until that process execs, and a file open for writing cannot be
    /// executed.
    fn open_past_a_forks_copy_of_the_stand_in(&self, name: &WindowName) -> Result<OpenModule, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match self.pair.open_module(name.clone(), None) {
                Err(why) if why.contains("Text file busy") && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                opened => return opened,
            }
        }
    }

    /// Opens `name` and takes the window's end of its link.
    fn open(&self, name: &WindowName) -> (PeerId, Their) {
        let opened = self.open_past_a_forks_copy_of_the_stand_in(name).unwrap();
        let OpenModule::Spawned(peer) = opened else {
            panic!("{opened:?}");
        };
        self.processes
            .borrow_mut()
            .push(self.pair.module_process(name).expect("the window is registered"));
        let end = self.window_ends.borrow_mut().pop().expect("a link was made for it");
        let (handle, inbound) = LinkHandle::start(end, PresenceFlag::default()).unwrap();
        (peer, Their { handle, inbound })
    }

    /// Says hello for the stand-in `started`, under `says`.
    fn say_hello(&self, their: &Their, started: &WindowName, says: &WindowName) {
        let process = self.pair.module_process(started).unwrap();
        let hello = hello(crate::version::fingerprint(), says, process.pid(), process.start());
        their.handle.send(hello, Vec::new()).unwrap();
    }

    /// Opens `name` and finishes its handshake; the descriptors its `welcome` carried.
    fn link(&self, name: &WindowName) -> (PeerId, Their, Vec<OwnedFd>) {
        let (peer, their) = self.open(name);
        self.say_hello(&their, name, name);
        let link = self.pair.module_link(name).unwrap();
        assert!(pump_until(&link, || link.linked()));
        let fds = welcome_fds(&their.inbound);
        (peer, their, fds)
    }

    fn peers(&self) -> Vec<(PeerId, WindowName)> {
        self.wm.query(|wm| wm.peer_names(WinId::MAIN))
    }

    fn fire_timers(&self) {
        let timers: Vec<_> = self.timers.borrow_mut().drain(..).collect();
        for timer in timers {
            timer();
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        for process in self.processes.borrow().iter() {
            let _ = process.end(Duration::from_secs(10)).map(|ending| ending.join());
        }
    }
}

fn welcome_fds(inbound: &Receiver<Inbound>) -> Vec<OwnedFd> {
    match inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
        Inbound::Msg {
            msg: Msg::Welcome { window, .. },
            fds,
        } => {
            assert_eq!(window, "1");
            fds
        }
        other => panic!("expected welcome, got {other:?}"),
    }
}

/// The next message `want` takes, past what the attach said; `None` when none comes.
fn next_that<T>(inbound: &Receiver<Inbound>, within: Duration, want: impl Fn(Msg) -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match inbound.recv_timeout(left) {
            Ok(Inbound::Msg { msg, .. }) => {
                if let Some(found) = want(msg) {
                    return Some(found);
                }
            }
            Ok(other) => panic!("{other:?}"),
            Err(_) => return None,
        }
    }
}

/// A linked link to pump, for a test whose link of interest has just gone from the registry.
fn pump_all(links: &[&LinkDriver]) {
    for link in links {
        link.pump();
    }
}

fn chat(n: u8) -> WindowName {
    WindowName::Chat(n)
}

fn markdown() -> WindowName {
    WindowName::Page("markdown".into())
}

#[test]
fn peers_start_above_the_pairs_own_and_are_never_reused() {
    let mut windows = ModuleWindows::default();
    let first = windows.next_peer();
    assert_eq!(first, PeerId(16));
    assert_eq!(windows.next_peer(), PeerId(17));
    assert_eq!(windows.next_peer(), PeerId(18));
}

#[test]
fn two_module_windows_link_and_each_gets_its_own_peer() {
    let group = group("two-module-windows", Duration::from_secs(30));
    let (first, _chat_end, _) = group.link(&chat(1));
    let (second, _page_end, _) = group.link(&markdown());
    assert_ne!(first, second);
    assert!(first.0 >= 16 && second.0 >= 16, "{first:?} {second:?}");
    assert_eq!(group.peers(), vec![(first, chat(1)), (second, markdown())]);
    assert_eq!(group.pair.module_names(), vec![chat(1), markdown()]);
    assert_eq!(group.pair.module_peer(&chat(1)), Some(first));
    assert_eq!(group.pair.module_peer(&markdown()), Some(second));
    assert_eq!(*group.linked.borrow(), vec![(chat(1), first), (markdown(), second)]);
    let link = group.pair.module_link(&chat(1)).unwrap();
    let process = group.pair.module_process(&chat(1)).unwrap();
    assert_eq!(
        link.partner().map(|p| (p.pid, p.start)),
        Some((process.pid(), process.start()))
    );
    assert!(group.pair.link().is_none(), "window 2's slot is not a module window's");
}

#[test]
fn a_hello_with_another_windows_name_is_refused() {
    let group = group("hello-other-name", Duration::from_secs(30));
    let (_, their) = group.open(&chat(1));
    let link = group.pair.module_link(&chat(1)).unwrap();
    let process = group.pair.module_process(&chat(1)).unwrap();
    group.say_hello(&their, &chat(1), &chat(2));
    assert!(pump_until(&link, || !link.is_up()));
    assert!(group.linked.borrow().is_empty(), "the link is not made");
    assert!(group.peers().is_empty());
    assert!(group.pair.module_names().is_empty(), "the name is free again");
    {
        let failed = group.failed.borrow();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, chat(1));
        assert!(failed[0].1.contains("refused"), "{failed:?}");
    }
    let status = process
        .wait_reaped(Duration::from_secs(10))
        .expect("the child is ended");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGTERM)
    );
    match their.inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
        Inbound::Msg {
            msg: Msg::Bye { why },
            fds,
        } => {
            assert!(fds.is_empty(), "no lock goes to a refused window");
            assert!(why.contains("calls itself"), "{why}");
        }
        other => panic!("expected bye, got {other:?}"),
    }
}

#[test]
fn a_good_hello_queued_behind_a_refused_one_makes_no_link() {
    let group = group("hello-after-refusal", Duration::from_secs(30));
    let (_, their) = group.open(&chat(1));
    let link = group.pair.module_link(&chat(1)).unwrap();
    group.say_hello(&their, &chat(1), &chat(2));
    group.say_hello(&their, &chat(1), &chat(1));
    assert!(pump_until(&link, || !link.is_up()));
    link.pump();
    assert!(group.linked.borrow().is_empty());
}

#[test]
fn a_module_window_that_misses_its_handshake_is_ended_and_others_stay_linked() {
    let group = group("missed-handshake", Duration::ZERO);
    let (first, first_end, _) = group.link(&chat(1));
    let (_silent_peer, _silent_end) = group.open(&chat(2));
    let silent = group.pair.module_process(&chat(2)).unwrap();
    let silent_link = group.pair.module_link(&chat(2)).unwrap();
    group.fire_timers();
    assert!(!silent_link.is_up(), "its link is closed");
    let status = silent
        .wait_reaped(Duration::from_secs(10))
        .expect("reaped through its pidfd");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGTERM)
    );
    assert_eq!(group.pair.module_names(), vec![chat(1)]);
    assert_eq!(group.peers(), vec![(first, chat(1))]);
    {
        let failed = group.failed.borrow();
        assert_eq!(
            failed.len(),
            1,
            "only the window that missed it is reported: {failed:?}"
        );
        assert_eq!(failed[0].0, chat(2));
        assert!(failed[0].1.contains("handshake"), "{failed:?}");
    }
    let link = group.pair.module_link(&chat(1)).unwrap();
    assert!(link.linked(), "the window that linked stays");
    assert!(
        group
            .pair
            .module_process(&chat(1))
            .unwrap()
            .wait_reaped(Duration::from_millis(200))
            .is_none(),
        "and runs on"
    );
    link.send(Msg::Presence { held: true }, Vec::new()).unwrap();
    let heard = next_that(&first_end.inbound, Duration::from_secs(5), |msg| {
        matches!(msg, Msg::Presence { held: true }).then_some(())
    });
    assert!(heard.is_some(), "its link still carries messages");
    let (again, _again_end) = group.open(&chat(2));
    assert!(again.0 > first.0, "a window started again is a new peer");
}

#[test]
fn a_module_window_that_dies_before_hello_is_forgotten() {
    let group = group("dies-before-hello", Duration::from_secs(30));
    let (peer, their) = group.open(&chat(1));
    let link = group.pair.module_link(&chat(1)).unwrap();
    drop(their);
    assert!(pump_until(&link, || link.ended().is_some()));
    assert!(group.pair.module_names().is_empty());
    assert_eq!(*group.ended.borrow(), vec![(chat(1), peer)]);
    assert!(group.peers().is_empty());
    {
        let failed = group.failed.borrow();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, chat(1));
        assert!(failed[0].1.contains("ended before it linked"), "{failed:?}");
    }
    let (again, _their) = group.open(&chat(1));
    assert!(again.0 > peer.0);
    // The first start's timer fires late, and finds a window of that name that is not the one it was started for:
    // it is not reported again, which would make the controller give up the new start.
    let first_timer = group.timers.borrow_mut().remove(0);
    first_timer();
    assert_eq!(group.pair.module_names(), vec![chat(1)]);
    assert_eq!(group.failed.borrow().len(), 1, "{:?}", group.failed.borrow());
}

#[test]
fn one_module_windows_link_end_detaches_only_its_peer() {
    let group = group("one-link-ends", Duration::from_secs(30));
    let (first, _first_end, _) = group.link(&chat(1));
    let (second, second_end, _) = group.link(&chat(2));
    let (link, other) = (
        group.pair.module_link(&chat(2)).unwrap(),
        group.pair.module_link(&chat(1)).unwrap(),
    );
    drop(second_end);
    assert!(pump_until(&link, || link.ended() == Some(LinkEnd::Eof)));
    pump_all(&[&link, &other]);
    assert_eq!(group.peers(), vec![(first, chat(1))]);
    assert_eq!(*group.ended.borrow(), vec![(chat(2), second)]);
    assert_eq!(group.pair.module_names(), vec![chat(1)]);
    assert!(other.linked());
    assert!(
        group.failed.borrow().is_empty(),
        "a window that linked and ended later did not fail to start: {:?}",
        group.failed.borrow()
    );
}

#[test]
fn opening_an_open_module_window_raises_it() {
    let group = group("raise", Duration::from_secs(30));
    let (peer, their, _) = group.link(&chat(1));
    let raised = group.pair.open_module(chat(1), Some("keys".into()));
    assert_eq!(raised, Ok(OpenModule::Raised(peer)));
    assert_eq!(group.processes.borrow().len(), 1);
    assert!(group.window_ends.borrow().is_empty(), "no second link was made");
    // The stand-in writes its line in its own time, and the link never waited for it: the test said its hello.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let started = loop {
        let text = std::fs::read_to_string(&group.report).unwrap_or_default();
        if !text.is_empty() {
            break text;
        }
        assert!(std::time::Instant::now() < deadline, "the stand-in never ran");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(started.lines().count(), 1, "nothing else was spawned");
    let focus = next_that(&their.inbound, Duration::from_secs(5), |msg| match msg {
        Msg::Focus { target, token } => Some((target, token)),
        _ => None,
    });
    assert_eq!(focus, Some((FocusTarget::Window, Some("keys".to_string()))));
}

#[test]
fn opening_a_window_still_in_its_handshake_starts_nothing_more() {
    let group = group("raise-early", Duration::from_secs(30));
    let (peer, _their) = group.open(&chat(1));
    assert_eq!(group.pair.open_module(chat(1), None), Ok(OpenModule::Raised(peer)));
    assert!(group.window_ends.borrow().is_empty(), "no second link was made");
}

#[test]
fn a_second_unpaired_or_alone_window_refuses_to_open_module_windows() {
    let root = Path::new("/p");
    let refused = |pair: &Pair| {
        assert_eq!(
            pair.open_module(chat(1), None),
            Err(MODULE_WINDOWS_ARE_WINDOW_1S.to_string())
        );
        assert!(pair.module_names().is_empty());
    };
    refused(&Pair::full(Ok(None), root, false, false));
    refused(&Pair::full(Err(std::io::Error::other("no state")), root, false, false));
    let (_one, two) = fdpass::seqpacket_pair().unwrap();
    refused(&Pair::second(two, root).unwrap());
    let (_one, two) = fdpass::seqpacket_pair().unwrap();
    refused(&Pair::module(two, chat(2), root).unwrap());
}

#[test]
fn window_2_is_not_opened_as_a_module_window() {
    let group = group("not-window-2", Duration::from_secs(30));
    assert!(group.pair.open_module(WindowName::Second, None).is_err());
    assert!(group.pair.module_names().is_empty());
}

#[test]
fn every_module_window_gets_the_group_lock() {
    let group = group("group-lock", Duration::from_secs(30));
    let (_, _chat_end, chat_fds) = group.link(&chat(1));
    let (_, _page_end, page_fds) = group.link(&markdown());
    let lock_file = std::fs::metadata(group.dir.0.join(pair_lock_name(Path::new("/p")))).unwrap();
    for fds in [chat_fds, page_fds] {
        assert_eq!(fds.len(), 1, "one descriptor in each welcome");
        let copy = std::fs::File::from(fds.into_iter().next().unwrap()).metadata().unwrap();
        assert_eq!((copy.dev(), copy.ino()), (lock_file.dev(), lock_file.ino()));
    }
}

#[test]
fn end_peer_ends_the_module_window_it_names_and_no_other() {
    let group = group("end-peer", Duration::from_secs(30));
    let _first = group.link(&chat(1));
    let (second, _second_end, _) = group.link(&chat(2));
    let (ended, kept) = (
        group.pair.module_process(&chat(2)).unwrap(),
        group.pair.module_process(&chat(1)).unwrap(),
    );
    LinkedWindows::end_peer(&*group.pair, WinId::MAIN, second).unwrap();
    assert!(ended.wait_reaped(Duration::from_secs(10)).is_some());
    assert!(kept.wait_reaped(Duration::from_millis(200)).is_none());
    assert!(
        LinkedWindows::end_peer(&*group.pair, WinId::MAIN, PeerId(99)).is_err(),
        "a peer nobody holds ends nothing"
    );
}

#[test]
fn a_module_window_started_again_is_told_so_and_gets_no_token() {
    let group = group("restored-module", Duration::from_secs(30));
    LinkedWindows::restore(&*group.pair, WinId::MAIN, &chat(3)).unwrap();
    group
        .processes
        .borrow_mut()
        .push(group.pair.module_process(&chat(3)).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let argv = loop {
        match std::fs::read_to_string(&group.report) {
            Ok(text) if !text.is_empty() => break text,
            _ => assert!(std::time::Instant::now() < deadline, "the stand-in never ran"),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        argv.trim(),
        format!("linked-window --restore --module chat:3 3 c3 {}", group.dir.1.display())
    );
}

#[test]
fn each_module_window_hears_what_this_window_holds_once() {
    let group = group("held", Duration::from_secs(30));
    let (_, first_end, _) = group.link(&chat(1));
    let (_, second_end, _) = group.link(&chat(2));
    let held: BTreeSet<String> = ["terminal#2".to_string()].into();
    group.pair.say_held_to_modules(&held);
    group.pair.say_held_to_modules(&held);
    for end in [&first_end, &second_end] {
        let away = |msg| match msg {
            Msg::Away { ids } => Some(ids),
            _ => None,
        };
        assert_eq!(
            next_that(&end.inbound, Duration::from_secs(5), away),
            Some(vec!["terminal#2".to_string()])
        );
        assert_eq!(
            next_that(&end.inbound, Duration::from_millis(100), away),
            None,
            "an unchanged set is not said again"
        );
    }
}

#[test]
fn a_module_windows_held_set_goes_to_the_controller_and_not_the_pair_file() {
    use crate::linked_window::pair_files::PairWriter;
    use eitri_core::layout::ModuleId;
    use eitri_core::wm::PeerEvent;
    let group = group("module-away", Duration::from_secs(30));
    let posted: Rc<RefCell<Vec<PeerEvent>>> = Rc::default();
    {
        let posted = posted.clone();
        let wm = Rc::downgrade(&group.wm);
        group.pair.set_hooks(PairHooks {
            linked: Rc::new(move |name, peer, link| {
                link.attach_controller(WinId::MAIN, peer, name.clone(), &wm.upgrade().unwrap());
            }),
            post: Rc::new(move |event| posted.borrow_mut().push(event)),
            ended: Rc::new(|_, _, _, _| {}),
        });
    }
    // With window 2 open every change of the file is written at once, so a module window's would show.
    let file = eitri_core::window_link::pair::PairFile {
        window2_open: true,
        ..Default::default()
    };
    let files = &group.dir.0;
    group
        .pair
        .set_pair_writer(PairWriter::new(files, Path::new("/p"), file, true));
    let (peer, their, _) = group.link(&chat(1));
    let away = Msg::Away {
        ids: vec!["terminal#2".into()],
    };
    let monitor = Msg::Monitor {
        connector: "DP-1".into(),
        description: "Left".into(),
    };
    their.handle.send(away, Vec::new()).unwrap();
    their.handle.send(monitor, Vec::new()).unwrap();
    let link = group.pair.module_link(&chat(1)).unwrap();
    assert!(pump_until(&link, || posted.borrow().len() == 2));
    assert_eq!(
        *posted.borrow(),
        vec![
            PeerEvent::Held {
                peer,
                ids: vec![ModuleId::parse("terminal#2").unwrap()]
            },
            PeerEvent::Monitor {
                peer,
                connector: "DP-1".into()
            }
        ]
    );
    let mut flushed = false;
    group
        .pair
        .with_pair_writer(|writer| flushed = writer.flush(Duration::from_secs(5)));
    assert!(flushed);
    let name = eitri_core::window_link::pair::pair_file_name(Path::new("/p"));
    assert!(!files.join(name).exists(), "the pair file names no module window");
}

#[test]
fn a_module_window_says_hello_with_its_name_and_links_on_welcome() {
    let dir = Scratch::new("module-side");
    let root = Path::new("/p");
    let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(two, markdown(), root).unwrap();
    let linked: Rc<RefCell<Vec<(WindowName, PeerId)>>> = Rc::default();
    let ended: Rc<RefCell<Vec<(WindowName, PeerId)>>> = Rc::default();
    {
        let (linked, ended) = (linked.clone(), ended.clone());
        pair.set_hooks(PairHooks {
            linked: Rc::new(move |name, peer, _| linked.borrow_mut().push((name.clone(), peer))),
            post: Rc::new(|_| {}),
            ended: Rc::new(move |name, peer, _, _| ended.borrow_mut().push((name.clone(), peer))),
        });
    }
    let (handle, inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    let said = inbound.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(&said, Inbound::Msg { msg: Msg::Hello { window, .. }, .. } if window == "p:markdown"),
        "{said:?}"
    );
    handle
        .send(
            crate::linked_window::spawn::welcome(Default::default(), Vec::new()),
            vec![lock.share().unwrap()],
        )
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));
    assert_eq!(*linked.borrow(), vec![(markdown(), HUB_PEER)]);
    assert!(pair.is_second(), "it has no editor and holds none of the pair's files");
    assert!(!pair.holds_pair());
    drop(handle);
    assert!(pump_until(&link, || link.ended().is_some()));
    assert_eq!(*ended.borrow(), vec![(markdown(), HUB_PEER)]);
}

/// A hub-tiled window 1 whose link to the chat window `c1` (peer 16) is the test's own: the window's end is returned.
fn hub_with_a_chat_window() -> (
    Rc<Driver<Controller>>,
    Rc<eitri_core::wm::fake::FakeHost>,
    LinkDriver,
    LinkHandle,
    Receiver<Inbound>,
) {
    use crate::linked_window::driver::tests::{controller_in, pair};
    let (wm, host) = controller_in(eitri_core::wm::window::Tiling::Hub);
    let (driver, theirs, their_inbound) = pair();
    driver.attach_controller(WinId::MAIN, PeerId(16), chat(1), &wm);
    (wm, host, driver, theirs, their_inbound)
}

#[test]
fn a_raise_of_a_module_window_goes_over_its_link_with_a_token() {
    use eitri_core::keymap::Action;
    use eitri_core::wm::fake::Logged;
    use eitri_core::wm::{HostEvent, HostOp, Origin, PeerEvent};
    let (wm, host, driver, _theirs, their_inbound) = hub_with_a_chat_window();
    wm.run(
        WinId::MAIN,
        Action::Module(eitri_core::layout::ModuleId::agent()),
        Origin::Key { keycode: 0 },
    );
    let asked = host
        .log()
        .iter()
        .any(|logged| matches!(logged, Logged::Op(HostOp::RaisePeer { peer, .. }, _) if *peer == PeerId(16)));
    assert!(asked, "the chat key asks the host to raise the open chat window");
    // The host's answer carries the activation token it made from window 1's surface.
    wm.post(HostEvent::Peer(PeerEvent::Raised {
        peer: PeerId(16),
        token: Some("surface-token".into()),
    }));
    driver.pump();
    let focus = next_that(&their_inbound, Duration::from_secs(5), |msg| match msg {
        Msg::Focus { target, token } => Some((target, token)),
        _ => None,
    });
    assert_eq!(focus, Some((FocusTarget::Window, Some("surface-token".to_string()))));
}

#[test]
fn a_hub_ask_from_a_module_window_reaches_window_1s_controller() {
    use eitri_core::window_link::HubAsk;
    use eitri_core::wm::PeerEvent;
    let group = group("hub-ask", Duration::from_secs(30));
    let posted: Rc<RefCell<Vec<PeerEvent>>> = Rc::default();
    {
        let posted = posted.clone();
        let wm = Rc::downgrade(&group.wm);
        group.pair.set_hooks(PairHooks {
            linked: Rc::new(move |name, peer, link| {
                link.attach_controller(WinId::MAIN, peer, name.clone(), &wm.upgrade().unwrap());
            }),
            post: Rc::new(move |event| posted.borrow_mut().push(event)),
            ended: Rc::new(|_, _, _, _| {}),
        });
    }
    let (peer, their, _) = group.link(&chat(2));
    let ask = HubAsk::TabBreak {
        agent: eitri_core::agent_backend::AgentId::Claude,
        session: "s1".into(),
        back: false,
    };
    their
        .handle
        .send(
            Msg::HubAsk {
                ask: ask.clone(),
                token: Some("their-token".into()),
            },
            Vec::new(),
        )
        .unwrap();
    let link = group.pair.module_link(&chat(2)).unwrap();
    assert!(pump_until(&link, || !posted.borrow().is_empty()));
    assert_eq!(
        *posted.borrow(),
        vec![PeerEvent::HubAsk {
            peer,
            ask,
            token: Some("their-token".into())
        }]
    );
}

#[test]
fn focus_in_of_a_module_window_tells_window_1() {
    use eitri_core::wm::PeerEvent;
    // The module window's side: it tells window 1 once its link is up, and not before.
    let dir = Scratch::new("module-focus");
    let root = Path::new("/p");
    let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
    let (one, two) = fdpass::seqpacket_pair().unwrap();
    let pair = Pair::module(two, markdown(), root).unwrap();
    let (handle, inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
    assert!(matches!(
        inbound.recv_timeout(Duration::from_secs(5)).unwrap(),
        Inbound::Msg {
            msg: Msg::Hello { .. },
            ..
        }
    ));
    pair.say_window_focused();
    handle
        .send(
            crate::linked_window::spawn::welcome(Default::default(), Vec::new()),
            vec![lock.share().unwrap()],
        )
        .unwrap();
    let link = pair.link().unwrap();
    assert!(pump_until(&link, || link.linked()));
    pair.say_window_focused();
    let told = next_that(&inbound, Duration::from_secs(5), |msg| {
        matches!(msg, Msg::WindowFocused {}).then_some(())
    });
    assert_eq!(told, Some(()), "the message follows the link's coming up");

    // Window 1's side: the message is the controller's `WindowFocused` of that window's peer.
    let group = group("focus-in", Duration::from_secs(30));
    let posted: Rc<RefCell<Vec<PeerEvent>>> = Rc::default();
    {
        let posted = posted.clone();
        let wm = Rc::downgrade(&group.wm);
        group.pair.set_hooks(PairHooks {
            linked: Rc::new(move |name, peer, link| {
                link.attach_controller(WinId::MAIN, peer, name.clone(), &wm.upgrade().unwrap());
            }),
            post: Rc::new(move |event| posted.borrow_mut().push(event)),
            ended: Rc::new(|_, _, _, _| {}),
        });
    }
    let (peer, their, _) = group.link(&chat(3));
    their.handle.send(Msg::WindowFocused {}, Vec::new()).unwrap();
    let link = group.pair.module_link(&chat(3)).unwrap();
    assert!(pump_until(&link, || !posted.borrow().is_empty()));
    assert_eq!(*posted.borrow(), vec![PeerEvent::WindowFocused { peer }]);
    // Any window but a module window says nothing.
    let main = Pair::full(Ok(None), root, false, false);
    main.say_window_focused();
}

#[test]
fn a_message_for_a_peer_goes_over_that_peers_link() {
    let group = group("send", Duration::from_secs(30));
    let (peer, their, _) = group.link(&chat(1));
    let (other, their_other, _) = group.link(&chat(2));
    let run = Msg::RunAction {
        name: "tab.select".into(),
        opts: vec![("n".into(), 2.into())],
    };
    LinkedWindows::send(&*group.pair, WinId::MAIN, other, run.clone()).unwrap();
    let got = next_that(&their_other.inbound, Duration::from_secs(5), |msg| {
        Some(msg).filter(|m| *m == run)
    });
    assert_eq!(got, Some(run));
    let quiet = next_that(&their.inbound, Duration::from_millis(200), |msg| {
        matches!(msg, Msg::RunAction { .. }).then_some(())
    });
    assert_eq!(quiet, None, "the other window heard nothing");
    assert_ne!(peer, other);
    assert!(
        LinkedWindows::send(&*group.pair, WinId::MAIN, PeerId(99), Msg::WindowFocused {}).is_err(),
        "a peer nobody holds is refused"
    );
}

#[test]
fn a_window_the_start_opens_keeps_the_keys_in_window_1() {
    let group = group("keep-keys", Duration::from_secs(30));
    LinkedWindows::open_module(&*group.pair, WinId::MAIN, &chat(2), Some("ignored".into()), true).unwrap();
    group
        .processes
        .borrow_mut()
        .push(group.pair.module_process(&chat(2)).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let argv = loop {
        match std::fs::read_to_string(&group.report) {
            Ok(text) if !text.is_empty() => break text,
            _ => assert!(std::time::Instant::now() < deadline, "the stand-in never ran"),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        argv.trim(),
        format!("linked-window --restore --module chat:2 3 c2 {}", group.dir.1.display()),
        "it is started as one started again is, with no token"
    );
    // Open already: left as it is, nothing started and nobody raised.
    LinkedWindows::open_module(&*group.pair, WinId::MAIN, &chat(2), None, true).unwrap();
    assert_eq!(std::fs::read_to_string(&group.report).unwrap().lines().count(), 1);
    // A key's open takes the token, and a window that was never opened is started with it.
    LinkedWindows::open_module(&*group.pair, WinId::MAIN, &chat(3), Some("keys".into()), false).unwrap();
    group
        .processes
        .borrow_mut()
        .push(group.pair.module_process(&chat(3)).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let all = loop {
        let text = std::fs::read_to_string(&group.report).unwrap_or_default();
        if text.lines().count() == 2 {
            break text;
        }
        assert!(std::time::Instant::now() < deadline, "the second stand-in never ran");
        std::thread::sleep(Duration::from_millis(10));
    };
    let second = all.lines().nth(1).unwrap();
    assert_eq!(
        second,
        format!("linked-window --module chat:3 3 c3 {} keys", group.dir.1.display())
    );
}

/// What `their` window heard when its window 1 closed the group.
fn heard_bye(their: &Their) -> Option<String> {
    next_that(&their.inbound, Duration::from_secs(5), |msg| match msg {
        Msg::Bye { why } => Some(why),
        _ => None,
    })
}

#[test]
fn closing_window_1_ends_every_module_window() {
    let group = group("group-close", Duration::from_secs(30));
    let deaf = group.dir.1.join("deaf");
    group.stand_ins_run(&format!("trap '' TERM\n: > '{}'\nexec sleep 30", deaf.display()));
    let (_, stubborn, _) = group.link(&chat(2));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !deaf.exists() {
        assert!(std::time::Instant::now() < deadline, "the stand-in never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    group.stand_ins_run("exec sleep 30");
    let (_, plain, _) = group.link(&chat(1));
    // Still in its handshake: it has no peer, and it goes with the rest.
    let (_, starting) = group.open(&markdown());
    let plain_process = group.pair.module_process(&chat(1)).unwrap();
    let stubborn_process = group.pair.module_process(&chat(2)).unwrap();
    let starting_process = group.pair.module_process(&markdown()).unwrap();

    group
        .pair
        .end_group_within(Duration::from_millis(50), Duration::from_millis(50));

    for their in [&plain, &stubborn, &starting] {
        assert_eq!(heard_bye(their).as_deref(), Some("window 1 closed"));
    }
    assert!(group.pair.module_names().is_empty(), "nothing of the group is left");
    let signal = |process: &SecondProcess| {
        let status = process.wait_reaped(Duration::from_secs(10)).expect("reaped");
        std::os::unix::process::ExitStatusExt::signal(&status)
    };
    assert_eq!(signal(&plain_process), Some(libc::SIGTERM));
    assert_eq!(signal(&starting_process), Some(libc::SIGTERM));
    assert_eq!(
        signal(&stubborn_process),
        Some(libc::SIGKILL),
        "it ignored the end it was told"
    );
    assert!(group.ended.borrow().is_empty(), "window 1 asked for these ends");
}

#[test]
fn a_stopped_window_of_a_closing_group_is_killed() {
    let group = group("group-close-stopped", Duration::from_secs(30));
    group.stand_ins_run("kill -STOP $$\nexec sleep 30");
    let (_, their, _) = group.link(&chat(1));
    let process = group.pair.module_process(&chat(1)).unwrap();
    let stat = format!("/proc/{}/stat", process.pid());
    let stopped = || {
        let stat = std::fs::read_to_string(&stat).unwrap_or_default();
        stat.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('T'))
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !stopped() {
        assert!(std::time::Instant::now() < deadline, "the stand-in never stopped");
        std::thread::sleep(Duration::from_millis(5));
    }

    group
        .pair
        .end_group_within(Duration::from_millis(50), Duration::from_millis(50));

    assert_eq!(heard_bye(&their).as_deref(), Some("window 1 closed"));
    let status = process.wait_reaped(Duration::from_secs(10)).expect("reaped");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL),
        "a stopped process holds its SIGTERM pending"
    );
}

#[test]
fn a_window_that_closes_after_window_1s_bye_is_sent_no_signal() {
    let group = group("group-close-polite", Duration::from_secs(30));
    let told = group.dir.1.join("told");
    group.stand_ins_run(&format!(
        "i=0\nwhile [ ! -e '{}' ] && [ $i -lt 3000 ]; do sleep 0.01; i=$((i+1)); done\nexit 7",
        told.display()
    ));
    let (_, their, _) = group.link(&chat(1));
    let process = group.pair.module_process(&chat(1)).unwrap();

    group
        .pair
        .end_group_within(Duration::from_secs(60), Duration::from_secs(60));

    assert_eq!(heard_bye(&their).as_deref(), Some("window 1 closed"));
    std::fs::write(&told, "").unwrap();
    let status = process.wait_reaped(Duration::from_secs(10)).expect("reaped");
    assert_eq!(status.code(), Some(7), "{status:?}");
}

#[test]
fn a_module_window_crash_takes_only_that_window() {
    let group = group("crash", Duration::from_secs(30));
    let (first, first_end, _) = group.link(&chat(1));
    let (second, _second_end, _) = group.link(&chat(2));
    let crashed = group.pair.module_process(&chat(1)).unwrap();
    let link = group.pair.module_link(&chat(1)).unwrap();

    crashed.end(Duration::ZERO).unwrap().join().unwrap();
    drop(first_end);
    assert!(pump_until(&link, || link.ended().is_some()));

    assert_eq!(link.ended(), Some(LinkEnd::Eof));
    assert_eq!(*group.ended.borrow(), vec![(chat(1), first)]);
    assert_eq!(group.peers(), vec![(second, chat(2))], "its peer is detached");
    assert_eq!(group.pair.module_names(), vec![chat(2)]);
    assert!(group.failed.borrow().is_empty(), "it had linked, so no start failed");
    assert!(group.pair.module_link(&chat(2)).unwrap().linked(), "the other stays");
    assert!(
        group
            .pair
            .module_process(&chat(2))
            .unwrap()
            .wait_reaped(Duration::from_millis(200))
            .is_none(),
        "and runs on"
    );
    let (again, _again_end) = group.open(&chat(1));
    assert!(again.0 > second.0, "its key starts it again, as a new peer");
}

#[test]
fn each_window_says_its_own_close() {
    let group = group("close-words", Duration::from_secs(30));
    assert_eq!(group.pair.closed_words(), "window 1 closed");
}

mod hand_over;
mod tabs;
