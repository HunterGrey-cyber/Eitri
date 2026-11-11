//! How the two windows link: window 1 starts window 2 and answers its `hello`, window 2 says `hello` and waits for
//! `welcome`.
//!
//! Window 1 makes the socketpair, starts its own executable as window 2 with the other end, and expects a `hello`
//! from exactly that child, of exactly this build: the child's pid and start time name it even if another process
//! somehow held the other end, and the build fingerprint keeps two builds from sharing one pair's state. A `hello`
//! that does not match is answered with `bye` and the link closes. A matching one is answered with `welcome`,
//! which carries a copy of the pair lock's descriptor, so the lock stays held while either window lives. A window 2
//! that has not finished the handshake by its deadline has failed to start: its link is closed and its process ended
//! (SIGTERM, then SIGKILL after a short grace), through a pidfd opened while it was still this window's unreaped
//! child, so nothing but that process can ever be signalled. The same ending is what the user asks for when window 2
//! stops answering in the middle of a move (`HostOp::EndPeer`); window 2 did not start window 1 and refuses it.
//!
//! A terminal's shell that moved to the other window is still this process's child, whose exit only this process can
//! read: [`watch_moved_child`] waits for it off the GTK thread, and the window tells whichever window runs that shell
//! (`child_exited`). The other window's `child_exited` reaches this window's terminals through the hook set with
//! [`Pair::set_child_exited`].
//!
//! Window 1 starts its chat and page windows the same way, one link and one process each ([`Pair::open_module`], kept in
//! [`ModuleWindows`]): the same handshake and deadline, the same end through a pidfd, and a copy of the same lock. A
//! module window's `hello` carries its own name, and a refused one ends the window, since it has no pair to go on
//! without. The windows of a group are a star: each links to window 1 and to nothing else. A window 1 that came back
//! for orphaned module windows holds them the same way ([`Pair::relinked_group`]), each with a pidfd it could open only
//! by name, since another process started them.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent::child::PidFd;

use eitri_core::window_link::fdpass;
use eitri_core::window_link::link::{LinkHandle, PresenceFlag};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::pair::{GroupClaim, PairLock};
use eitri_core::window_link::start::{spawn_linked, LinkedFlags, SECOND_WINDOW};
use eitri_core::window_link::{FocusTarget, Msg, Summary, TurnEdge};
use eitri_core::wm::feedback::TAB_ALREADY_MOVING_TEXT;
use eitri_core::wm::PeerId;

use super::closes::WindowCloses;
use super::driver::{LinkDriver, Partner, HANDSHAKE_WITHIN};
use super::group_tabs::{GroupTabs, TabReports, REPORT_AFTER};
use super::hand_over::{self, TabHolder, TabHolderSlot};
use super::pair_files::PairWriter;
use super::registry::{ModuleLink, ModuleWindows, OpenModule, MODULE_WINDOWS_ARE_WINDOW_1S};
use super::relink::{self, HeldNow, OrphanSocket, Relinked};
use super::{LinkEnd, WindowNew, WindowRole, KEPT_WORDS, NO_OTHER_WINDOW};
use crate::agent_panel::TabAnswer;
use crate::project_stamp::{ProjectStamp, StampSlot};
use eitri_core::saved_tabs::{SavedTab, SavedTabs};

/// Window 1's name on the link.
pub const FIRST_WINDOW: &str = "1";

/// What window 1 calls window 2.
pub const SECOND_PEER: PeerId = PeerId(2);
/// What window 2 and a module window call window 1: their one other window.
pub const HUB_PEER: PeerId = PeerId(1);

/// The `hello` window 1 accepts: from the child it started under that name, of its own build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub build: String,
    pub name: WindowName,
    pub pid: u32,
    pub start: u64,
}

/// Checks a started window's `hello` against what window 1 started; the partner it names, or why it is refused.
pub fn check_hello(msg: &Msg, expected: &Expected) -> Result<Partner, String> {
    let Msg::Hello {
        build,
        window,
        pid,
        start,
        ..
    } = msg
    else {
        return Err(format!("the window's first message was `{}`, not hello", msg.kind()));
    };
    if *build != expected.build {
        return Err(format!(
            "the window started here is another build ({build}, this one is {})",
            expected.build
        ));
    }
    let name = expected.name.text();
    if *window != name {
        return Err(format!("the window started as {name} calls itself window {window:?}"));
    }
    if (*pid, *start) != (expected.pid, expected.start) {
        return Err(format!(
            "hello came from process {pid} (started at {start}), not the window started here ({}, {})",
            expected.pid, expected.start
        ));
    }
    Ok(Partner {
        pid: *pid,
        start: *start,
    })
}

/// A started window's `hello`: its build, its name, its process, and nothing confirmed yet about its own presence
/// (the partner counts it as another window until it says so). It is said before the window's panel exists, so no
/// turn of it runs yet.
pub fn hello(build: String, name: &WindowName, pid: u32, start: u64) -> Msg {
    Msg::Hello {
        build,
        window: name.text(),
        pid,
        start,
        presence_ready: false,
        running: Vec::new(),
        summary: Summary::default(),
    }
}

/// Window 1's `welcome`, with the turns running here now (`running`, as started edges). The feeds' payloads, the
/// holders and the counts are filled in by what owns them; a window that has none yet sends none. Its presence is
/// confirmed afterwards, as `presence{held: true}`, once its presence thread has answered for the turns running
/// then: nothing here waits for that thread.
pub fn welcome(holders: BTreeMap<String, String>, running: Vec<TurnEdge>) -> Msg {
    Msg::Welcome {
        window: FIRST_WINDOW.to_string(),
        presence_ready: false,
        running,
        summary: Summary::default(),
        holders,
        context: None,
        theme: None,
        keys: None,
    }
}

/// What the window does at the moments of the link it cares about here: once linked (to say what it holds), when
/// the other window asks it to take the keys, and once the link ended without its asking (to bring back what the
/// other window held). Set once the window's grid exists; nothing on the link is read before the main loop runs.
#[derive(Clone)]
pub struct PairHooks {
    pub linked: OnLinked,
    /// Pair messages report their intent and facts through the controller's queue.
    pub post: Rc<dyn Fn(eitri_core::wm::PeerEvent)>,
    pub ended: OnEnded,
}

/// What runs once a link is made: the window at the started end, what this window calls the other one, the link.
pub type OnLinked = Rc<dyn Fn(&WindowName, PeerId, &LinkDriver)>;
/// What a handshake runs once it has linked: [`OnLinked`] with its window and peer settled.
pub type LinkMade = Rc<dyn Fn(&LinkDriver)>;
/// The turns running in this window now, as started edges, for its `welcome`.
pub type RunningTurns = Rc<dyn Fn() -> Vec<TurnEdge>>;
/// What runs once a link ended without this window asking, named as in [`OnLinked`].
pub type OnEnded = Rc<dyn Fn(&WindowName, PeerId, &LinkDriver, &LinkEnd)>;
/// What a `child_exited` from the other window is given to: the shell's pid and start time, and its raw wait status.
pub type OnChildExited = Rc<dyn Fn(u32, u64, Option<i32>)>;
/// What a `run_action` from window 1 is given to, in a module window: the action's name and the options that rebuild it.
pub type OnRunAction = Rc<dyn Fn(String, Vec<(String, serde_json::Value)>)>;
/// What window 1 does when the window 2 it started again at its launch does not come back, with why.
pub type OnRestoreFailed = Rc<dyn Fn(&str)>;
/// What window 1 does when the module window it started did not come up: the window's name and why.
pub type OnModuleFailed = Rc<dyn Fn(&WindowName, &str)>;

type Hooks = Rc<RefCell<Option<PairHooks>>>;

/// Runs a closure once `Duration` has passed, on the main loop.
pub type After = Rc<dyn Fn(Duration, Box<dyn FnOnce()>)>;
/// Makes the two ends of a new link: this window's, then the started window's.
pub type LinkEnds = Rc<dyn Fn() -> io::Result<(OwnedFd, OwnedFd)>>;

/// How this window starts another window of the group: the executable it runs, how long a window it starts has for
/// its handshake, measured by `after`, and how the link is made. A window runs its own build ([`Starts::default`]);
/// the tests run stand-ins with timers of their own, and keep a copy of the started end to speak for them.
#[derive(Clone)]
pub struct Starts {
    pub exe: PathBuf,
    pub handshake_within: Duration,
    pub after: After,
    pub link_ends: LinkEnds,
}

impl Default for Starts {
    fn default() -> Starts {
        Starts {
            // The running executable, not the installed path: an upgrade that replaced the file still starts this build.
            exe: PathBuf::from("/proc/self/exe"),
            handshake_within: HANDSHAKE_WITHIN,
            after: Rc::new(|within, run| {
                gtk4::glib::timeout_add_local_once(within, run);
            }),
            link_ends: Rc::new(fdpass::seqpacket_pair),
        }
    }
}

/// The window's role and what it needs to carry out the window verbs: the pair lock while it holds one, and the
/// project root window 2 is started for.
pub struct Pair {
    role: RefCell<WindowRole>,
    lock: Option<Rc<PairLock>>,
    root: PathBuf,
    hooks: Hooks,
    /// The activation token window 2 was started with, until its first show takes it.
    activation: RefCell<Option<String>>,
    /// The pair file as this window knows it: written by window 1 while it holds the pair, by window 2 once
    /// orphaned. `None` in a window with no part in a pair.
    pair_writer: Rc<RefCell<Option<PairWriter>>>,
    /// Whether this window's chats run the legacy backend, so a window 2 it starts is told to run it too.
    legacy_backend: std::cell::Cell<bool>,
    /// Whether this launch is `--clean`, so a window 2 it starts is clean too.
    clean: std::cell::Cell<bool>,
    /// The turns running here, for the `welcome` of a window 2 this window starts.
    running: Rc<RefCell<Option<RunningTurns>>>,
    /// The window 2 this window started, while it may run: what `HostOp::EndPeer` ends.
    second: RefCell<Option<Rc<SecondProcess>>>,
    /// Window 1 only: the chat and page windows it started, for as long as each one's link lasts.
    modules: Rc<RefCell<ModuleWindows>>,
    /// Where the other window's `child_exited` goes.
    child_exited: Rc<RefCell<Option<OnChildExited>>>,
    /// A module window: where window 1's `run_action` goes.
    peer_action: Rc<RefCell<Option<OnRunAction>>>,
    /// A module window: what lets its tabs go and takes tabs up, for a hand-over.
    tab_holder: TabHolderSlot,
    /// A module window: the tabs it let go until window 1 can be sure to hear of them.
    unwritten: Rc<hand_over::UnwrittenTabs>,
    /// A module window: its tabs on their way to window 1, which saves them.
    tab_reports: Rc<TabReports>,
    /// A module window an end signal is closing, so its `bye` asks window 1 to keep it.
    keep_window: Rc<std::cell::Cell<bool>>,
    /// Window 1, waiting for its chat windows' last tabs: where the write that follows is announced, once the close has
    /// asked. `Some(None)` from the ask on.
    last_tabs_wait: RefCell<Option<Option<std::sync::mpsc::Sender<()>>>>,
    /// Window 2 only: window 1 started it again with `--restore`.
    restored: std::cell::Cell<bool>,
    /// Window 1 only: a window 2 started again at this launch has not linked yet, so its failure to come back is
    /// still to be handled (once).
    restore_pending: Rc<std::cell::Cell<bool>>,
    restore_failed: Rc<RefCell<Option<OnRestoreFailed>>>,
    /// Window 1 only: what runs when a module window it started never linked.
    module_failed: Rc<RefCell<Option<OnModuleFailed>>>,
    closes: Rc<WindowCloses>,
    /// Window 2 and a module window: the pair lock their `welcome` carried, kept for as long as the window lives, so
    /// an orphan can hand a copy to the window 1 that comes back for it.
    second_lock: Rc<RefCell<Option<PairLock>>>,
    /// What marks this window's activations on the lock, once it has a lock to mark.
    stamp: Rc<StampSlot>,
    /// Window 2 only, while orphaned: the socket a new window 1 links through.
    orphan: RefCell<Option<Orphan>>,
    /// A window 1 that linked to an orphan before it was built: what the orphan said it holds, for the layout phase.
    relinked: RefCell<Option<BTreeMap<String, String>>>,
    /// That window's link was made before its hooks were set: the `linked` hook is still to run, once they are.
    relink_unannounced: std::cell::Cell<bool>,
    /// A window 1 that came back for module windows: the pair's files stay as `eitri` mode left them.
    relinked_modules: std::cell::Cell<bool>,
    /// Window 1's claim on the group, held for as long as this window runs.
    group_claim: RefCell<Option<GroupClaim>>,
    /// How a window this one starts is started.
    starts: RefCell<Starts>,
    /// Window 2 only: the editor window it started that has neither linked nor ended yet, so a second key press
    /// does not start another.
    editor_start: RefCell<Option<relink::StartedEditor>>,
    /// The port this window's next shell gets, the very cell its `ShellPort` reads. In a window with an editor it is
    /// the listener's own; in window 2 it is window 1's, while a link lasts.
    ide_port: Rc<std::cell::Cell<Option<u16>>>,
    /// Window 1 only: the port on the command line of the window 2 it started, until that window's first link. A
    /// listener lost in between has to reach that window, which nothing else can correct.
    port_on_command_line: std::cell::Cell<Option<u16>>,
}

/// An orphan's listener and the connection it is answering, if one is.
struct Orphan {
    socket: Rc<OrphanSocket>,
    watch: Option<gtk4::glib::SourceId>,
    held: HeldNow,
    /// A connection that has not said a matching hello yet. One at a time: another arriving meanwhile is closed,
    /// unless this one's handshake is overdue.
    candidate: Option<LinkDriver>,
}

impl Pair {
    /// A full window: window 1 of the pair with the lock `taken`, else unpaired or alone
    /// ([`WindowRole::of_full_window`]). `legacy_backend`: this launch's chats run the legacy backend; `clean`: this
    /// launch is `--clean`.
    pub fn full(taken: io::Result<Option<PairLock>>, root: &Path, legacy_backend: bool, clean: bool) -> Rc<Pair> {
        let (role, lock) = WindowRole::of_full_window(taken);
        let pair = Pair::new(role, lock, root);
        pair.legacy_backend.set(legacy_backend);
        pair.clean.set(clean);
        pair
    }

    /// Window 2, on its checked link end: says `hello` at once and links on `welcome`.
    pub fn second(sock: OwnedFd, root: &Path) -> io::Result<Rc<Pair>> {
        let hooks: Hooks = Rc::default();
        let lock: Rc<RefCell<Option<PairLock>>> = Rc::default();
        let name = WindowName::Second;
        let stamp: Rc<StampSlot> = Rc::default();
        let made = linked_hook(&hooks, name.clone(), HUB_PEER);
        let link = link_second(sock, &name, made, lock.clone(), stamp.clone())?;
        // The link's handler fills the slots once `welcome` comes.
        let pair = Pair::with_hooks(WindowRole::Second { link }, None, root, hooks, lock, stamp);
        Ok(pair)
    }

    /// The page window `name` beside the person's own nvim (`eitri page`), with `link` to that nvim: no window 1, no
    /// pair lock and no link to another window of Eitri's, so nothing on the pair's side is wired.
    pub fn beside(name: WindowName, root: &Path, link: Rc<eitri_core::companion::link::CompanionLink>) -> Rc<Pair> {
        Pair::new(WindowRole::Beside { name, link }, None, root)
    }

    /// The module window `name`, on its checked link end: says `hello` under that name at once and links on `welcome`.
    pub fn module(sock: OwnedFd, name: WindowName, root: &Path) -> io::Result<Rc<Pair>> {
        let hooks: Hooks = Rc::default();
        let lock: Rc<RefCell<Option<PairLock>>> = Rc::default();
        let stamp: Rc<StampSlot> = Rc::default();
        let made = linked_hook(&hooks, name.clone(), HUB_PEER);
        let link = link_second(sock, &name, made, lock.clone(), stamp.clone())?;
        Ok(Pair::with_hooks(
            WindowRole::Module { name, link },
            None,
            root,
            hooks,
            lock,
            stamp,
        ))
    }

    /// A window 1 that found the pair lock held and linked to the orphaned window 2 holding it ([`relink::relink`]):
    /// window 1 of the pair, with that lock and that link. Its `linked` hook runs once the window sets its hooks.
    pub fn relinked(relinked: Relinked, root: &Path, legacy_backend: bool, clean: bool) -> Rc<Pair> {
        let Relinked {
            name: _,
            handle,
            inbound,
            presence,
            lock,
            partner,
            running,
            holders,
        } = relinked;
        let link = LinkDriver::new(handle, inbound);
        link.set_presence(presence);
        link.set_partner(partner);
        link.set_partner_running(running);
        link.set_linked();
        let pair = Pair::new(WindowRole::Main { link: Some(link) }, Some(lock), root);
        pair.legacy_backend.set(legacy_backend);
        pair.clean.set(clean);
        *pair.relinked.borrow_mut() = Some(holders);
        pair.relink_unannounced.set(true);
        println!("[link] linked to the waiting window 2 (process {})", partner.pid);
        pair
    }

    /// A window 1 that found the pair lock held and linked to the orphaned module windows holding it
    /// ([`relink::relink_all`]): window 1 of the group, with no window 2 and each orphan in the registry under a fresh
    /// peer. One copy of the lock is kept, the first; every copy is one open file description, so closing the others
    /// keeps it held. Their `linked` hooks run once the window sets its hooks. With nobody to link to it runs unpaired.
    pub fn relinked_group(relinked: Vec<Relinked>, root: &Path, legacy_backend: bool, clean: bool) -> Rc<Pair> {
        let mut lock = None;
        let mut holders = BTreeMap::new();
        let mut links = Vec::with_capacity(relinked.len());
        for one in relinked {
            lock.get_or_insert(one.lock);
            holders.extend(one.holders);
            let link = LinkDriver::new(one.handle, one.inbound);
            link.set_presence(one.presence);
            link.set_partner(one.partner);
            link.set_partner_running(one.running);
            link.set_linked();
            links.push((one.name, one.partner, link));
        }
        let Some(lock) = lock else {
            return Pair::full(Ok(None), root, legacy_backend, clean);
        };
        let pair = Pair::new(WindowRole::Main { link: None }, Some(lock), root);
        pair.legacy_backend.set(legacy_backend);
        pair.clean.set(clean);
        for (name, partner, link) in links {
            // Gone already, then: its link ends with this copy of it, and there is nothing to hold.
            let process = match SecondProcess::adopted(partner) {
                Ok(process) => Rc::new(process),
                Err(err) => {
                    eprintln!("[link] the waiting window {} is gone: {err}", name.text());
                    continue;
                }
            };
            let peer = pair.modules.borrow_mut().next_peer();
            pair.wire_module(&name, peer, &link, Rc::new(std::cell::Cell::new(true)));
            println!(
                "[link] linked to the waiting window {} (process {})",
                name.text(),
                partner.pid
            );
            pair.modules.borrow_mut().insert(
                name,
                ModuleLink {
                    peer,
                    link,
                    process,
                    said: Default::default(),
                },
            );
        }
        *pair.relinked.borrow_mut() = Some(holders);
        pair.relink_unannounced.set(true);
        pair.relinked_modules.set(true);
        pair
    }

    fn new(role: WindowRole, lock: Option<PairLock>, root: &Path) -> Rc<Pair> {
        Pair::with_hooks(role, lock, root, Rc::default(), Rc::default(), Rc::default())
    }

    fn with_hooks(
        role: WindowRole,
        lock: Option<PairLock>,
        root: &Path,
        hooks: Hooks,
        second_lock: Rc<RefCell<Option<PairLock>>>,
        stamp: Rc<StampSlot>,
    ) -> Rc<Pair> {
        let modules: Rc<RefCell<ModuleWindows>> = Rc::default();
        let pair = Rc::new(Pair {
            role: RefCell::new(role),
            lock: lock.map(Rc::new),
            root: root.to_path_buf(),
            hooks,
            activation: RefCell::new(None),
            pair_writer: Rc::default(),
            legacy_backend: std::cell::Cell::new(false),
            clean: std::cell::Cell::new(false),
            running: Rc::default(),
            second: RefCell::new(None),
            modules: modules.clone(),
            child_exited: Rc::default(),
            peer_action: Rc::default(),
            tab_holder: Rc::default(),
            unwritten: Rc::default(),
            tab_reports: Rc::default(),
            keep_window: Rc::default(),
            last_tabs_wait: RefCell::default(),
            restored: std::cell::Cell::new(false),
            restore_pending: Rc::default(),
            restore_failed: Rc::default(),
            module_failed: Rc::default(),
            closes: WindowCloses::new(Rc::downgrade(&modules)),
            second_lock,
            stamp,
            orphan: RefCell::new(None),
            relinked: RefCell::new(None),
            relink_unannounced: std::cell::Cell::new(false),
            relinked_modules: std::cell::Cell::new(false),
            group_claim: RefCell::default(),
            starts: RefCell::new(Starts::default()),
            editor_start: RefCell::new(None),
            ide_port: Rc::default(),
            port_on_command_line: std::cell::Cell::new(None),
        });
        if let Some(link) = pair.link() {
            pair.wire(&link);
        }
        pair
    }

    /// The token window 1 handed over with the start, so window 2's first show gets the keys.
    pub fn set_activation(&self, token: Option<String>) {
        *self.activation.borrow_mut() = token;
    }

    pub fn take_activation(&self) -> Option<String> {
        self.activation.borrow_mut().take()
    }

    /// Window 2: whether window 1 started it again with `--restore`.
    pub fn set_restored(&self, restored: bool) {
        self.restored.set(restored);
    }

    pub fn restored(&self) -> bool {
        self.restored.get()
    }

    /// Window 2: the flags it was started with, `--legacy` and `--clean`, which an editor window it starts runs
    /// with too, so the pair it re-links to keeps one backend and one posture towards the remembered state.
    pub fn set_launch_flags(&self, legacy_backend: bool, clean: bool) {
        self.legacy_backend.set(legacy_backend);
        self.clean.set(clean);
    }

    /// The cell the window's `ShellPort` reads, so the pair and the panes hold one value.
    pub fn ide_port_cell(&self) -> Rc<std::cell::Cell<Option<u16>>> {
        self.ide_port.clone()
    }

    #[cfg(test)]
    pub fn ide_port(&self) -> Option<u16> {
        self.ide_port.get()
    }

    /// The port this window's next shell gets. Nothing that runs is told.
    pub fn set_ide_port(&self, port: Option<u16>) {
        self.ide_port.set(port);
    }

    /// The listener's port is `port` from here on: a window 2 on a link that is up hears it now, and one still in its
    /// handshake hears it from [`Pair::say_ide_port`].
    pub fn ide_port_changed(&self, port: Option<u16>) {
        self.set_ide_port(port);
        if let Some(link) = self.link().filter(LinkDriver::linked) {
            self.send_ide_port(&link, port);
        }
    }

    /// A new link to window 2 is up: it hears the port as it stands. With no port there is nothing to tell, unless that
    /// window was started with one. A window with no listener, so a window with the key off, says nothing.
    pub fn say_ide_port(&self, link: &LinkDriver) {
        let port = self.ide_port.get();
        let started_with_one = self.port_on_command_line.take().is_some();
        if self.owns_editor() && (port.is_some() || started_with_one) {
            self.send_ide_port(link, port);
        }
    }

    fn send_ide_port(&self, link: &LinkDriver, port: Option<u16>) {
        if let Err(err) = link.send(Msg::IdePort { port }, Vec::new()) {
            eprintln!("[link] window 2 could not be told the editor's port: {err}");
        }
    }

    /// How a window this one starts is started: stand-ins in the tests.
    #[cfg(test)]
    pub(crate) fn set_starts(&self, starts: Starts) {
        *self.starts.borrow_mut() = starts;
    }

    /// What this window does when the window 2 it starts again at its launch does not come back.
    pub fn on_restore_failed(&self, hook: OnRestoreFailed) {
        *self.restore_failed.borrow_mut() = Some(hook);
    }

    /// What this window does when a module window it started never links: its handshake was missed, its hello was
    /// refused or its link ended first. The controller drops what waited for it and the name can be started again.
    pub fn on_module_failed(&self, hook: OnModuleFailed) {
        *self.module_failed.borrow_mut() = Some(hook);
    }

    /// The window 2 started again at this launch did not come back, for `why`: the hook runs, once, and `true` is
    /// returned. `false` when no restore is waiting (none was asked, it linked, or its failure was handled).
    pub fn fail_restore(&self, why: &str) -> bool {
        fail_restore(&self.restore_pending, &self.restore_failed, why)
    }

    /// Where the turns running in this window are read when it welcomes a window 2.
    pub fn set_running_turns(&self, running: RunningTurns) {
        *self.running.borrow_mut() = Some(running);
    }

    /// Where the other window's `child_exited` goes, from here on: the window's terminals.
    pub fn set_child_exited(&self, hook: OnChildExited) {
        *self.child_exited.borrow_mut() = Some(hook);
    }

    /// Where window 1's `run_action` goes, from here on: the module window's runner. Only a module window takes one.
    pub fn set_peer_action(&self, hook: OnRunAction) {
        *self.peer_action.borrow_mut() = Some(hook);
    }

    /// What lets this module window's tabs go and takes tabs up, from here on.
    pub fn set_tab_holder(&self, holder: Rc<dyn TabHolder>) {
        *self.tab_holder.borrow_mut() = Some(holder);
    }

    /// Window 1 keeps its group's tabs from here on, each module window telling it theirs.
    pub fn keep_group_tabs(&self, tabs: GroupTabs) {
        self.modules.borrow_mut().keep_tabs(tabs);
    }

    /// Window 1 is closing: the receiver reports once the group's tabs file holds what it was last handed, which comes
    /// after the last tabs the chat windows were asked for.
    pub fn finish_group_tabs(&self) -> Option<std::sync::mpsc::Receiver<()>> {
        if let Some(announce) = self.last_tabs_wait.borrow_mut().as_mut() {
            let (told, heard) = std::sync::mpsc::channel();
            *announce = Some(told);
            return Some(heard);
        }
        self.modules.borrow_mut().finish_tabs()
    }

    /// This module window's tabs are `tabs` now: window 1 hears at once, or with what else changes in the moment after
    /// its last report.
    pub fn tabs_changed(self: &Rc<Self>, tabs: SavedTabs) {
        let link = self.link().filter(LinkDriver::linked);
        if self.tab_reports.changed(tabs, link.as_ref()) {
            let pair = Rc::downgrade(self);
            let after = self.starts.borrow().after.clone();
            after(
                REPORT_AFTER,
                Box::new(move || {
                    if let Some(pair) = pair.upgrade() {
                        pair.tab_reports
                            .timer_ended(pair.link().filter(LinkDriver::linked).as_ref());
                    }
                }),
            );
        }
    }

    /// This module window's link is up: window 1 hears the tabs it holds, if it has told of any.
    pub fn say_tabs(&self) {
        if !self.is_module() {
            return;
        }
        if let Some(link) = self.link().filter(LinkDriver::linked) {
            self.tab_reports.linked(&link);
        }
    }

    /// This module window's panel answered a hand-over: window 1 hears it. A tab it let go that window 1 will not hear
    /// of is taken up here again, as nobody else will be given it; one that stays, or did not come up, is said here too.
    pub fn answer_hand_over(&self, answer: TabAnswer) {
        let holder = self.tab_holder.borrow().clone();
        if let (TabAnswer::Kept { why, .. } | TabAnswer::NotAdopted { why, .. }, Some(holder)) = (&answer, &holder) {
            holder.kept(why.clone());
        }
        let released = match &answer {
            TabAnswer::Released { tab, .. } => Some(tab.clone()),
            _ => None,
        };
        let link = self.link().filter(LinkDriver::linked);
        let written = link.and_then(|link| {
            link.send_confirmed(hand_over::answer_message(answer), Vec::new())
                .map_err(|err| eprintln!("[link] window 1 could not be told how a hand-over went: {err}"))
                .ok()
        });
        match (released, written) {
            (Some(tab), Some(written)) => self.unwritten.hold(written, tab),
            (Some(tab), None) => take_lost_tabs(vec![tab], &self.tab_holder),
            (None, _) => {}
        }
        take_lost_tabs(self.unwritten.settle(), &self.tab_holder);
    }

    /// A release queued on a link, for a test that gives its answer itself.
    #[cfg(test)]
    pub(crate) fn hold_released(&self, written: std::sync::mpsc::Receiver<io::Result<()>>, tab: SavedTab) {
        self.unwritten.hold(written, tab);
    }

    /// A module window's toplevel gained the keyboard: window 1 is told, for the window that holds the keys. Any
    /// other window, and a link that is not up, say nothing.
    pub fn say_window_focused(&self) {
        if !self.is_module() {
            return;
        }
        let Some(link) = self.link().filter(LinkDriver::linked) else {
            return;
        };
        if let Err(err) = link.send(Msg::WindowFocused {}, Vec::new()) {
            eprintln!("[link] window 1 could not be told this window has the keys: {err}");
        }
    }

    /// What the window does on the link's events, from here on. A link made before the window was built (a
    /// re-link) is announced to them now.
    pub fn set_hooks(&self, hooks: PairHooks) {
        *self.hooks.borrow_mut() = Some(hooks);
        if self.relink_unannounced.replace(false) {
            if let Some(link) = self.link().filter(LinkDriver::linked) {
                linked_hook(&self.hooks, self.link_name(), self.link_peer())(&link);
            }
            for name in self.module_names() {
                let known = self.modules.borrow().get(&name).map(|m| (m.peer, m.link.clone()));
                if let Some((peer, link)) = known.filter(|(_, link)| link.linked()) {
                    linked_hook(&self.hooks, name.clone(), peer)(&link);
                    self.modules.borrow().give_tabs(&name);
                }
            }
        }
    }

    /// What the orphans this window linked to at its start said they hold; `None` for a window that did not re-link.
    pub fn relink_holders(&self) -> Option<BTreeMap<String, String>> {
        self.relinked.borrow().clone()
    }

    /// Starts watching the links of the module windows this window came back for.
    pub fn watch_module_links(&self) {
        for module in self.modules.borrow().links() {
            module.link.watch();
        }
    }

    pub fn owns_editor(&self) -> bool {
        self.role.borrow().owns_editor()
    }

    /// Keeps `claim` until this window ends, whichever way it came to lead the group.
    pub fn hold_group_claim(&self, claim: Option<GroupClaim>) {
        *self.group_claim.borrow_mut() = claim;
    }

    /// Starts the stamp of the lock this window started with, if it has one: `root` for the window that took the lock,
    /// `None` for one that adopted a copy. An unpaired or lone window has none and does nothing.
    pub(crate) fn start_stamp(&self, root: Option<PathBuf>) {
        if let Some(lock) = &self.lock {
            self.stamp.fill(ProjectStamp::start(lock, root));
        }
    }

    /// What this window's activations go to.
    pub(crate) fn stamp(&self) -> Rc<StampSlot> {
        self.stamp.clone()
    }

    /// Whether this window holds the project's pair lock as its window 1: the one that reads the pair file at
    /// start and writes it while the pair runs.
    pub fn holds_pair(&self) -> bool {
        self.lock.is_some() && self.role.borrow().writes_main_files()
    }

    /// Whether this window reads the pair file at start and writes it. A window 1 that came back for module windows
    /// does not: they have no place in it, and `eitri` mode finds it as it was left.
    pub fn keeps_pair_file(&self) -> bool {
        self.holds_pair() && !self.relinked_modules.get()
    }

    /// The pair file's writer, from the window's start on.
    pub fn set_pair_writer(&self, writer: PairWriter) {
        *self.pair_writer.borrow_mut() = Some(writer);
    }

    /// Runs `f` on the pair file's writer, if this window has one.
    pub fn with_pair_writer(&self, f: impl FnOnce(&mut PairWriter)) {
        if let Some(writer) = self.pair_writer.borrow_mut().as_mut() {
            f(writer);
        }
    }

    /// Window 1's pair file once the link to a window 2 ended without this window asking.
    pub fn window2_link_ended(&self, link: &LinkDriver, end: &LinkEnd) {
        match end {
            // Window 2 closed: what it held is back here, and the next launch does not start it.
            LinkEnd::Bye { .. } => self.with_pair_writer(PairWriter::window2_closed),
            // No goodbye: window 2 may still run, orphaned, and keep the pair file itself.
            LinkEnd::Eof if link.partner().is_some() => self.with_pair_writer(|writer| {
                writer.hand_over();
            }),
            // No goodbye from a window 2 that never linked: it held nothing and never had the pair lock to keep the
            // file with, so this window goes on writing it.
            LinkEnd::Eof => {}
        }
    }

    /// Whether window 1 started this window: window 2 or a module window, neither of which has an editor or holds
    /// the pair's files.
    pub fn is_second(&self) -> bool {
        matches!(
            *self.role.borrow(),
            WindowRole::Second { .. } | WindowRole::Module { .. }
        )
    }

    /// The name of the chat or page window this is, a group's or one beside nvim; `None` in every other window,
    /// window 2 included.
    pub fn module_window(&self) -> Option<WindowName> {
        match &*self.role.borrow() {
            WindowRole::Module { name, .. } | WindowRole::Beside { name, .. } => Some(name.clone()),
            WindowRole::Alone | WindowRole::Main { .. } | WindowRole::Second { .. } | WindowRole::Unpaired => None,
        }
    }

    /// Whether this is a page window beside the person's own nvim: no window 1 started it, and it starts none.
    pub fn is_beside(&self) -> bool {
        self.role.borrow().is_beside()
    }

    /// A beside window's link to the person's nvim; `None` in every other window.
    pub fn beside_link(&self) -> Option<Rc<eitri_core::companion::link::CompanionLink>> {
        match &*self.role.borrow() {
            WindowRole::Beside { link, .. } => Some(link.clone()),
            _ => None,
        }
    }

    /// Whether this is a module window: window 1 decides its reverts.
    pub fn is_module(&self) -> bool {
        matches!(*self.role.borrow(), WindowRole::Module { .. })
    }

    pub fn is_unpaired(&self) -> bool {
        matches!(*self.role.borrow(), WindowRole::Unpaired)
    }

    /// Whether this window found no pair lock to take: nothing could link to it.
    pub fn is_alone(&self) -> bool {
        matches!(*self.role.borrow(), WindowRole::Alone)
    }

    /// Whether this window linked at its start to the module windows an earlier window 1 left behind.
    pub fn relinked_module_windows(&self) -> bool {
        self.relinked_modules.get()
    }

    pub fn pair_role(&self) -> eitri_core::window_link::pair::Role {
        self.role.borrow().pair_role()
    }

    pub fn wm_spec_role(&self) -> (eitri_core::wm::WindowRole, eitri_core::wm::EditorPresence) {
        self.role.borrow().wm_spec_role()
    }

    pub fn link(&self) -> Option<LinkDriver> {
        self.role.borrow().link().cloned()
    }

    /// The started window at the end of [`Pair::link`]: this window, when it is one, else window 2.
    fn link_name(&self) -> WindowName {
        match &*self.role.borrow() {
            WindowRole::Module { name, .. } | WindowRole::Beside { name, .. } => name.clone(),
            WindowRole::Alone | WindowRole::Main { .. } | WindowRole::Second { .. } | WindowRole::Unpaired => {
                WindowName::Second
            }
        }
    }

    /// What this window calls the window at the other end of [`Pair::link`].
    fn link_peer(&self) -> PeerId {
        if self.is_second() {
            HUB_PEER
        } else {
            SECOND_PEER
        }
    }

    /// What window 1 calls the module window `name`, while its link lasts.
    pub fn module_peer(&self, name: &WindowName) -> Option<PeerId> {
        self.modules.borrow().get(name).map(|module| module.peer)
    }

    /// The peer of the window that holds the plugin `owner`'s page when this window does not: its own page window while
    /// one is linked, window 2 otherwise.
    pub fn page_peer(&self, owner: &str) -> PeerId {
        self.module_peer(&WindowName::Page(owner.to_owned()))
            .unwrap_or(SECOND_PEER)
    }

    /// The module windows window 1 has started and whose links last.
    #[cfg_attr(not(test), allow(dead_code))] // The window verbs name windows through the controller.
    pub fn module_names(&self) -> Vec<WindowName> {
        self.modules.borrow().names()
    }

    /// Tells every linked module window what this window holds, each one only when it has not heard that set yet.
    pub fn say_held_to_modules(&self, held: &BTreeSet<String>) {
        for module in self.modules.borrow().links() {
            module.said.say(&module.link, held.clone());
        }
    }

    /// The module window `name`'s link, for a test that steps it.
    #[cfg(test)]
    pub(crate) fn module_link(&self, name: &WindowName) -> Option<LinkDriver> {
        self.modules.borrow().get(name).map(|module| module.link.clone())
    }

    /// The module window `name`'s process, for a test that speaks for it.
    #[cfg(test)]
    pub(crate) fn module_process(&self, name: &WindowName) -> Option<Rc<SecondProcess>> {
        self.modules.borrow().get(name).map(|module| module.process.clone())
    }

    fn wire(&self, link: &LinkDriver) {
        let hooks = self.hooks.clone();
        let first = !self.is_second();
        let (pending, failed) = (self.restore_pending.clone(), self.restore_failed.clone());
        let (name, peer) = (self.link_name(), self.link_peer());
        let port = self.ide_port.clone();
        link.on_end(move |link, end| {
            // The port was window 1's, which may be gone: a shell started from here on has none, whatever `ended`
            // goes on to do.
            if !first {
                port.set(None);
            }
            // A window 2 started again at this launch that ended before it linked did not come back: the
            // restore's own take-back runs instead.
            if first && link.partner().is_none() && fail_restore(&pending, &failed, "window 2 ended before it linked") {
                return;
            }
            let hook = hooks.borrow().as_ref().map(|h| h.ended.clone());
            if let Some(ended) = hook {
                ended(&name, peer, link, end);
            }
        });
        self.post_kind(link, "workspace", peer);
        if self.is_second() {
            let port = self.ide_port.clone();
            link.on("ide_port", move |_, msg, _| {
                if let Msg::IdePort { port: told } = msg {
                    port.set(told);
                }
            });
        }
        if self.is_module() {
            hand_over::serve(link, self.tab_holder.clone());
            // A write's answer wakes the link; the link's end answers every write still queued.
            let (unwritten, holder) = (self.unwritten.clone(), self.tab_holder.clone());
            link.on_wake(move |_| take_lost_tabs(unwritten.settle(), &holder));
            let (unwritten, holder) = (self.unwritten.clone(), self.tab_holder.clone());
            link.on_end(move |_, _| take_lost_tabs(unwritten.settle(), &holder));
            let (reports, holder, keep) = (
                self.tab_reports.clone(),
                self.tab_holder.clone(),
                self.keep_window.clone(),
            );
            link.on_closing(move |link| {
                let held = holder.borrow().clone();
                reports.closing(link, held.and_then(|held| last_tabs(&*held, keep.get())));
            });
            let (reports, holder) = (self.tab_reports.clone(), self.tab_holder.clone());
            link.on("group_closing", move |link, _, _| {
                let held = holder.borrow().clone();
                reports.closing(link, held.and_then(|held| held.tabs_to_keep()));
                if let Err(err) = link.send(Msg::TabsSaid {}, Vec::new()) {
                    eprintln!("[link] window 1 could not be told this window's last tabs are out: {err}");
                }
            });
        }
        // The pair file names each window's monitor; the other window's comes over the link.
        let other_window = if self.is_second() { FIRST_WINDOW } else { SECOND_WINDOW };
        self.wire_messages(link, peer, Some(other_window));
    }

    /// A module window's link in window 1, as [`Pair::wire`] does a window 2's, keyed by its peer: its end forgets the
    /// window, and the window's name is free to start again; a clean end after linking goes to [`WindowCloses`], which
    /// decides whether what is kept for it goes too. The pair file has no place for a module window, so what it holds and
    /// which monitor it is on are the controller's alone.
    fn wire_module(&self, name: &WindowName, peer: PeerId, link: &LinkDriver, was_linked: Rc<std::cell::Cell<bool>>) {
        let hooks = self.hooks.clone();
        let modules = Rc::downgrade(&self.modules);
        let window = name.clone();
        let failed = self.module_failed.clone();
        let closes = self.closes.clone();
        let after = self.starts.borrow().after.clone();
        link.on_end(move |link, end| {
            if forget_module(&modules, peer).is_some() && !was_linked.get() {
                module_failed(&failed, &window, &format!("its link ended before it linked ({end:?})"));
            }
            if was_linked.get() && end.closed_for_good() {
                closes.window_closed(window.clone(), &after);
            }
            let hook = hooks.borrow().as_ref().map(|h| h.ended.clone());
            if let Some(ended) = hook {
                ended(&window, peer, link, end);
            }
        });
        self.wire_messages(link, peer, None);
        for kind in ["tab_released", "tab_kept", "tab_adopted"] {
            let modules = Rc::downgrade(&self.modules);
            let window = name.clone();
            link.on(kind, move |_, msg, _| {
                if let Some(modules) = modules.upgrade() {
                    modules.borrow_mut().hand_over_heard(&window, msg);
                }
            });
        }
        let modules = Rc::downgrade(&self.modules);
        let window = name.clone();
        link.on("tabs_report", move |_, msg, _| {
            if let (Msg::TabsReport { tabs }, Some(modules)) = (msg, modules.upgrade()) {
                modules.borrow_mut().tabs_reported(&window, &tabs);
            }
        });
        let modules = Rc::downgrade(&self.modules);
        let window = name.clone();
        link.on("tabs_said", move |_, _, _| {
            if let Some(modules) = modules.upgrade() {
                modules.borrow_mut().last_tabs_said(&window);
            }
        });
    }

    /// Hands `kind`'s messages to the controller as `peer`'s events.
    fn post_kind(&self, link: &LinkDriver, kind: &'static str, peer: PeerId) {
        let hooks = self.hooks.clone();
        link.on(kind, move |_, msg, _| {
            post_event(&hooks, crate::gtk_host::peer::pair_event(peer, &msg));
        });
    }

    /// What every link of this window takes from `peer`. `pair_file` is the name the pair file has for that window,
    /// when it has one.
    fn wire_messages(&self, link: &LinkDriver, peer: PeerId, pair_file: Option<&'static str>) {
        self.post_kind(link, "summary", peer);
        self.post_kind(link, "focus", peer);
        // What a module window asks of window 1, and its toplevel taking the keys: the controller of the window that
        // holds the group decides both, any other refuses them.
        self.post_kind(link, "hub_ask", peer);
        self.post_kind(link, "window_focused", peer);
        let peer_action = self.peer_action.clone();
        link.on("run_action", move |_, msg, _| {
            let Msg::RunAction { name, opts } = msg else {
                return;
            };
            let hook = peer_action.borrow().clone();
            match hook {
                Some(hook) => hook(name, opts),
                None => eprintln!("[link] run_action {name}: not an action here"),
            }
        });
        let child_exited = self.child_exited.clone();
        link.on("child_exited", move |_, msg, _| {
            let Msg::ChildExited { pid, start, status } = msg else {
                return;
            };
            let hook = child_exited.borrow().clone();
            match hook {
                Some(hook) => hook(pid, start, status),
                None => println!("[link] the shell {pid} ended in the other window's terminal; nothing here runs it"),
            }
        });
        let hooks = self.hooks.clone();
        let pair_writer = self.pair_writer.clone();
        link.on("monitor", move |_, msg, _| {
            // Checked as the other window's input.
            if let Msg::Monitor { connector, description } = &msg {
                match super::pair_files::peer_monitor(connector, description) {
                    Some(monitor) => {
                        if let (Some(window), Some(writer)) = (pair_file, pair_writer.borrow_mut().as_mut()) {
                            writer.monitor(window, monitor);
                        }
                    }
                    None => eprintln!("[link] the other window names a monitor {connector:?}; not kept"),
                }
            }
            post_event(&hooks, crate::gtk_host::peer::pair_event(peer, &msg));
        });
        // What window 2 holds is what the pair file names it the holder of; window 2 hears window 1's held set
        // only to keep its own numbers apart.
        let records_holders = pair_file == Some(SECOND_WINDOW);
        let pair_writer = self.pair_writer.clone();
        let hooks = self.hooks.clone();
        link.on("away", move |_, msg, _| {
            let event = crate::gtk_host::peer::pair_event(peer, &msg);
            if let Msg::Away { ids } = msg {
                println!("[link] the other window holds {}", ids.join(", "));
                if records_holders {
                    if let Some(writer) = pair_writer.borrow_mut().as_mut() {
                        writer.window2_holds(&ids.iter().cloned().collect());
                    }
                }
            }
            post_event(&hooks, event);
        });
    }

    /// `window.new`: in window 1, starts window 2 (or raises it when it is up); `token` is the activation token
    /// the key press gave, so the window that opens gets the keys. Refused in an unpaired window and in one that
    /// cannot pair.
    pub fn window_new(&self, token: Option<String>) -> Result<WindowNew, String> {
        let plan = self.role.borrow().window_new();
        match &plan {
            WindowNew::Refuse(text) => return Err((*text).to_string()),
            WindowNew::Raise => {
                self.focus_other(token)?;
            }
            WindowNew::Spawn => {
                let Some(lock) = self.lock.clone() else {
                    return Err(super::ALONE_REFUSAL.to_string());
                };
                let starts = self.starts.borrow().clone();
                let (link, second) = start_second(
                    &starts,
                    &self.root,
                    token.as_deref(),
                    self.second_flags(false),
                    lock,
                    linked_hook(&self.hooks, WindowName::Second, SECOND_PEER),
                    self.running.clone(),
                    None,
                )
                .map_err(|e| format!("the second window could not be started: {e}"))?;
                *self.second.borrow_mut() = Some(second);
                self.wire(&link);
                *self.role.borrow_mut() = WindowRole::Main { link: Some(link) };
            }
        }
        Ok(plan)
    }

    /// Window 1's launch, when window 2 was open as the pair last ended: starts window 2 again with `--restore` and
    /// no activation token, so this window keeps the keys. Should it not come back -- its start fails here, it ends
    /// or never finishes its handshake -- the restore-failed hook gives this window back what it held for it.
    pub fn restore(&self) -> Result<(), String> {
        self.restore_pending.set(true);
        let started = self.start_restored();
        if let Err(why) = &started {
            self.fail_restore(why);
        }
        started
    }

    fn start_restored(&self) -> Result<(), String> {
        if !matches!(self.role.borrow().window_new(), WindowNew::Spawn) {
            return Err("window 2 runs already, or this window cannot start one".to_string());
        }
        let Some(lock) = self.lock.clone() else {
            return Err(super::ALONE_REFUSAL.to_string());
        };
        // Linked, it is restored: what it held is its own again, and nothing comes back here.
        let linked = {
            let pending = self.restore_pending.clone();
            let linked = linked_hook(&self.hooks, WindowName::Second, SECOND_PEER);
            Rc::new(move |link: &LinkDriver| {
                pending.set(false);
                linked(link);
            }) as LinkMade
        };
        let overdue = {
            let (pending, hook) = (self.restore_pending.clone(), self.restore_failed.clone());
            Rc::new(move || {
                fail_restore(&pending, &hook, "window 2 did not finish its handshake");
            }) as Rc<dyn Fn()>
        };
        let starts = self.starts.borrow().clone();
        let (link, second) = start_second(
            &starts,
            &self.root,
            None,
            self.second_flags(true),
            lock,
            linked,
            self.running.clone(),
            Some(overdue),
        )
        .map_err(|e| format!("the second window could not be started: {e}"))?;
        *self.second.borrow_mut() = Some(second);
        self.wire(&link);
        *self.role.borrow_mut() = WindowRole::Main { link: Some(link) };
        Ok(())
    }

    /// What a window this one starts is passed on.
    fn flags(&self, restore: bool) -> LinkedFlags {
        LinkedFlags {
            legacy: self.legacy_backend.get(),
            clean: self.clean.get(),
            restore,
            ide_port: None,
        }
    }

    /// What window 2 is passed on: [`Pair::flags`] and the port as it stands, so its first shell can have it. A
    /// module window has no terminal, so nothing that gets the port.
    fn second_flags(&self, restore: bool) -> LinkedFlags {
        let ide_port = self.ide_port.get();
        self.port_on_command_line.set(ide_port);
        LinkedFlags {
            ide_port,
            ..self.flags(restore)
        }
    }

    /// Window 1 opens the module window `name`: starts it under the next peer, or, when it is open, raises it with
    /// `token` (the key press's, so it gets the keys) and starts nothing. A window still in its handshake takes the keys
    /// with the token it was started with. Refused in every window but window 1.
    pub fn open_module(&self, name: WindowName, token: Option<String>) -> Result<OpenModule, String> {
        self.start_module(name, token, false)
    }

    fn start_module(&self, name: WindowName, token: Option<String>, restore: bool) -> Result<OpenModule, String> {
        if !matches!(*self.role.borrow(), WindowRole::Main { .. }) {
            return Err(MODULE_WINDOWS_ARE_WINDOW_1S.to_string());
        }
        if name == WindowName::Second {
            return Err("window 2 is not a module window".to_string());
        }
        let Some(lock) = self.lock.clone() else {
            return Err(MODULE_WINDOWS_ARE_WINDOW_1S.to_string());
        };
        let open = self.modules.borrow().get(&name).map(|m| (m.peer, m.link.clone()));
        if let Some((peer, link)) = open {
            if link.linked() {
                let focus = Msg::Focus {
                    target: FocusTarget::Window,
                    token,
                };
                link.send(focus, Vec::new())
                    .map_err(|e| format!("the window {} could not be asked: {e}", name.text()))?;
            }
            return Ok(OpenModule::Raised(peer));
        }
        self.closes.reopening(&name);
        let peer = self.modules.borrow_mut().next_peer();
        // A window that misses its handshake, or is refused at its hello, never links: the controller is told, or it
        // would count the window as starting for good.
        let forget = {
            let modules = Rc::downgrade(&self.modules);
            let (failed, name) = (self.module_failed.clone(), name.clone());
            Rc::new(move || {
                // Only a window still in the registry is reported: one whose link ended already was, and its name
                // may have been started again since.
                if forget_module(&modules, peer).is_some() {
                    module_failed(&failed, &name, "its handshake was not finished in time");
                }
            }) as Rc<dyn Fn()>
        };
        // A window refused at its hello has no pair to go on without, and nothing else ends it.
        let refused = {
            let modules = Rc::downgrade(&self.modules);
            let (failed, name) = (self.module_failed.clone(), name.clone());
            Rc::new(move || {
                if let Some(module) = forget_module(&modules, peer) {
                    if let Some(ending) = end_second(&module.process, END_GRACE) {
                        hold_until_finished(ending);
                    }
                    module_failed(&failed, &name, "its hello was refused");
                }
            }) as Rc<dyn Fn()>
        };
        // Whether the window linked, for the end of its link: one that ends after linking is the controller's
        // detach, one that ends before it is a start that failed.
        let was_linked = Rc::new(std::cell::Cell::new(false));
        let made = linked_hook(&self.hooks, name.clone(), peer);
        let linked: LinkMade = {
            let was_linked = was_linked.clone();
            let modules = Rc::downgrade(&self.modules);
            let name = name.clone();
            Rc::new(move |link| {
                was_linked.set(true);
                made(link);
                if let Some(modules) = modules.upgrade() {
                    modules.borrow().give_tabs(&name);
                    modules.borrow_mut().hand_over_target_linked(&name);
                }
            })
        };
        let starts = self.starts.borrow().clone();
        let (link, process) = start_window(
            &starts,
            &self.root,
            &name,
            token.as_deref(),
            self.flags(restore),
            lock,
            linked,
            self.running.clone(),
            Some(forget),
            Some(refused),
        )
        .map_err(|e| format!("the window {} could not be started: {e}", name.text()))?;
        self.wire_module(&name, peer, &link, was_linked);
        self.modules.borrow_mut().insert(
            name,
            ModuleLink {
                peer,
                link,
                process,
                said: Default::default(),
            },
        );
        Ok(OpenModule::Spawned(peer))
    }

    /// Raises the other window and gives it the keys. The window controller's `window.other` sends the same
    /// `focus{window}` itself, once the host has asked the activation token; this stays for the tests of the pair.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn window_other(&self, token: Option<String>) -> Result<(), String> {
        self.focus_other(token)
    }

    fn focus_other(&self, token: Option<String>) -> Result<(), String> {
        let missing = if self.is_second() {
            super::ORPHAN_NOTE
        } else {
            NO_OTHER_WINDOW
        };
        let link = self
            .link()
            .filter(LinkDriver::linked)
            .ok_or_else(|| missing.to_string())?;
        link.send(
            Msg::Focus {
                target: FocusTarget::Window,
                token,
            },
            Vec::new(),
        )
        .map_err(|e| format!("the other window could not be asked: {e}"))
    }
}

impl Pair {
    /// What this window's `bye` says of its own close. A module window that an end signal closes, or whose tabs on offer
    /// nobody answered, says it is kept: the user has not closed them.
    pub fn closed_words(&self) -> String {
        let unopened = || self.tab_holder.borrow().as_ref().and_then(|held| held.unopened());
        if self.is_module() && (self.keep_window.get() || unopened().is_some()) {
            return format!("window {} {KEPT_WORDS}", self.link_name().text());
        }
        let name = if self.is_second() {
            self.link_name().text()
        } else {
            FIRST_WINDOW.to_string()
        };
        format!("window {name} closed")
    }

    /// An end signal is closing this module window, which is not the user closing it.
    pub fn keep_this_window(&self) {
        self.keep_window.set(true);
    }

    /// Window 1 was asked to close: a module window that goes around now went with it, and stays for the next start.
    pub fn group_asked(&self) {
        let after = self.starts.borrow().after.clone();
        self.closes.group_asked(&after);
    }

    /// What window 1 keeps for a closed window goes with it: `hook` hears each one, when that is settled.
    pub fn on_window_forgotten(&self, hook: super::closes::OnForgotten) {
        self.closes.on_forgotten(hook);
    }

    /// The closes heard so far as they stand at `now`, for a test that has no wish to wait out the burst.
    #[cfg(test)]
    pub(crate) fn settle_closes_at(&self, now: std::time::Instant) {
        self.closes.settle(now);
    }

    /// Window 1 closes its group. The chat windows are asked for their last tabs first, while their links can carry
    /// them; then every module window hears `bye`, with the application held until each has gone. Only the module
    /// windows are told: window 2 outlives window 1.
    pub fn end_group(self: &Rc<Self>) {
        self.group_asked();
        if !self.modules.borrow_mut().ask_last_tabs() {
            self.end_group_now();
            return;
        }
        *self.last_tabs_wait.borrow_mut() = Some(None);
        self.await_last_tabs(Instant::now() + LAST_TABS_WITHIN, hold_application());
    }

    /// Comes back on a timer until every window asked has answered or `until` has passed: a window that cannot answer
    /// costs only what it had not said.
    fn await_last_tabs(self: &Rc<Self>, until: Instant, hold: Option<gtk4::gio::ApplicationHoldGuard>) {
        if self.modules.borrow_mut().last_tabs_owed() && Instant::now() < until {
            let pair = self.clone();
            let after = self.starts.borrow().after.clone();
            after(LAST_TABS_POLL, Box::new(move || pair.await_last_tabs(until, hold)));
            return;
        }
        let announce = self.last_tabs_wait.borrow_mut().take().flatten();
        self.end_group_now();
        let write = self.modules.borrow_mut().finish_tabs();
        match (announce, write) {
            (Some(announce), Some(write)) => {
                let relay = std::thread::Builder::new().name("group-tabs".into()).spawn(move || {
                    let _ = write.recv();
                    let _ = announce.send(());
                });
                if let Err(err) = relay {
                    eprintln!("[tabs] no thread to wait for the group's last write ({err}); not waiting");
                }
            }
            (Some(announce), None) => {
                let _ = announce.send(());
            }
            (None, _) => {}
        }
    }

    fn end_group_now(&self) {
        let told = eitri_core::wm::end_signals::told_within(END_GRACE, crate::end_signals::since_first_signal());
        self.end_group_within(END_GRACE, told);
    }

    /// Every module window hears `bye` and is seen off, with [`SecondProcess::end_unless_closed`]'s two bounds.
    pub(crate) fn end_group_within(&self, grace: Duration, told: Duration) {
        let windows = ModuleWindows::end_group(&self.modules, &self.closed_words());
        for process in windows {
            if let Some(ending) = end_closing(&process, grace, told) {
                hold_until_finished(ending);
            }
        }
    }

    /// Whether this window waits for a window 1 once its link ended like `end`. Window 2 outlives its window 1 however
    /// that ended. A module window waits only for one that died: a `bye` is the group closing, and it closes too.
    pub fn waits_for_relink_after(&self, end: &LinkEnd) -> bool {
        !(matches!(*self.role.borrow(), WindowRole::Module { .. }) && matches!(end, LinkEnd::Bye { .. }))
    }

    /// A window window 1 started, orphaned: binds the socket a window 1 starting for the project links through
    /// ([`relink`]), under this window's name, in `dir`, and watches it on the main context. `held` is read for the
    /// `welcome`. Nothing is bound in a window that window 1 did not start or that holds no pair lock, nor twice.
    pub fn wait_for_relink(self: &Rc<Self>, dir: &Path, held: HeldNow) -> Result<(), String> {
        if !self.is_second() {
            return Err("only a window window 1 started waits for a window 1".into());
        }
        if self.second_lock.borrow().is_none() {
            return Err("this window holds no pair lock to hand over".into());
        }
        if self.orphan.borrow().is_some() {
            return Ok(());
        }
        let name = self.link_name();
        let socket = Rc::new(OrphanSocket::bind_as(dir, &self.root, &name)?);
        let me = Rc::downgrade(self);
        let watch = neovide_editor::fd_watch::add_local(
            socket.raw_fd(),
            gtk4::glib::IOCondition::IN,
            gtk4::glib::ffi::G_PRIORITY_DEFAULT,
            move |_| match me.upgrade() {
                Some(pair) => {
                    pair.accept_relink();
                    gtk4::glib::ControlFlow::Continue
                }
                None => gtk4::glib::ControlFlow::Break,
            },
        );
        let watch = match watch {
            Ok(id) => Some(id),
            Err(_) => {
                eprintln!("[window] the re-link socket is watched off the main context's thread; nothing is accepted");
                None
            }
        };
        println!(
            "[window] window {} waits for a window 1 at {}",
            name.text(),
            socket.path().display()
        );
        *self.orphan.borrow_mut() = Some(Orphan {
            socket,
            watch,
            held,
            candidate: None,
        });
        Ok(())
    }

    /// Takes the connections waiting on the orphan's socket: the first is answered once it says hello, and any
    /// other is closed while it is being answered, unless its handshake is overdue.
    pub fn accept_relink(self: &Rc<Self>) {
        loop {
            let Some(socket) = self.orphan.borrow().as_ref().map(|orphan| orphan.socket.clone()) else {
                return;
            };
            let conn = match socket.accept() {
                Ok(Some(conn)) => conn,
                Ok(None) => return,
                Err(err) => {
                    eprintln!("[window] the re-link socket could not accept ({err})");
                    return;
                }
            };
            let busy = self
                .orphan
                .borrow()
                .as_ref()
                .and_then(|orphan| orphan.candidate.clone());
            if let Some(busy) = busy {
                if !busy.handshake_overdue(Instant::now()) {
                    eprintln!("[window] a window 1 is being linked already; another connection is closed");
                    continue;
                }
                busy.close("the handshake took too long");
            }
            let peer = relink::peer_of(&conn);
            let presence = not_yet_confirmed();
            let (handle, inbound) = match LinkHandle::start(conn, presence.clone()) {
                Ok(started) => started,
                Err(err) => {
                    eprintln!("[window] a link to the connecting window 1 could not be started ({err})");
                    continue;
                }
            };
            let link = LinkDriver::new(handle, inbound);
            link.set_presence(presence);
            link.set_handshake_deadline(Instant::now() + relink::RELINK_WITHIN);
            let me = Rc::downgrade(self);
            link.on("hello", move |link, msg, _| {
                if let Some(pair) = me.upgrade() {
                    pair.answer_relink(link, &msg, peer);
                }
            });
            let me = Rc::downgrade(self);
            link.on_end(move |link, _| {
                if let Some(pair) = me.upgrade() {
                    pair.forget_candidate(link);
                }
            });
            link.watch();
            if let Some(orphan) = self.orphan.borrow_mut().as_mut() {
                orphan.candidate = Some(link);
            }
        }
    }

    /// The connection being answered, for a test that steps it.
    #[cfg(test)]
    pub(crate) fn relink_candidate(&self) -> Option<LinkDriver> {
        self.orphan
            .borrow()
            .as_ref()
            .and_then(|orphan| orphan.candidate.clone())
    }

    fn forget_candidate(&self, link: &LinkDriver) {
        if let Some(orphan) = self.orphan.borrow_mut().as_mut() {
            if orphan.candidate.as_ref().is_some_and(|c| c.same(link)) {
                orphan.candidate = None;
            }
        }
    }

    /// A connected window 1's hello: refused with `bye`, or welcomed with the pair lock, after which that window
    /// writes the pair file and its link is this window's.
    fn answer_relink(self: &Rc<Self>, link: &LinkDriver, msg: &Msg, peer: Option<Partner>) {
        // The new window 1 writes the pair file once linked: another hello on that link is its input and changes
        // nothing here, least of all who writes the file.
        if link.linked() {
            eprintln!("[link] a second hello on a linked link; ignored");
            return;
        }
        // A welcome transfers the file to window 1: every write already running must have finished first.
        let mut quiet = true;
        self.with_pair_writer(|writer| quiet = writer.hand_over());
        self.answer_relink_after_hand_over(link, msg, peer, quiet);
    }

    pub(super) fn answer_relink_after_hand_over(
        self: &Rc<Self>,
        link: &LinkDriver,
        msg: &Msg,
        peer: Option<Partner>,
        quiet: bool,
    ) {
        if !quiet {
            self.with_pair_writer(PairWriter::keep_ownership);
            link.close("the pair file is still being written");
            self.forget_candidate(link);
            return;
        }
        let held = self.orphan.borrow().as_ref().map(|orphan| orphan.held.clone());
        let held = held.map(|held| held()).unwrap_or_default();
        let running = self
            .running
            .borrow()
            .clone()
            .map(|running| running())
            .unwrap_or_default();
        let welcome = relink::relink_welcome_as(&self.link_name(), &held, running);
        let answered = match self.second_lock.borrow().as_ref() {
            Some(lock) => relink::answer_relink_hello(link, msg, &crate::version::fingerprint(), peer, lock, welcome),
            None => {
                link.close("this window holds no pair lock to hand over");
                Err("no pair lock".to_string())
            }
        };
        match answered {
            Ok(partner) => {
                println!("[link] window 1 is back (process {}); linked to it", partner.pid);
                self.promote(link);
            }
            Err(_) => {
                self.with_pair_writer(PairWriter::take_over);
                self.forget_candidate(link);
            }
        }
    }

    /// The re-link is made: the socket goes, and the link is this window's, wired and announced as any link is.
    fn promote(self: &Rc<Self>, link: &LinkDriver) {
        if let Some(orphan) = self.orphan.borrow_mut().take() {
            // The watch goes before the socket it watches is closed.
            if let Some(watch) = orphan.watch {
                watch.remove();
            }
        }
        let name = self.link_name();
        *self.role.borrow_mut() = match name.clone() {
            WindowName::Second => WindowRole::Second { link: link.clone() },
            name => WindowRole::Module {
                name,
                link: link.clone(),
            },
        };
        // Linked: the editor window started from here is this window 1, or no longer matters; a later start is
        // judged by the link.
        self.editor_start.borrow_mut().take();
        self.wire(link);
        linked_hook(&self.hooks, name, HUB_PEER)(link);
    }
}

/// The window controller's `window.new`, carried out by the GTK host: what the pair started or raised is not the
/// controller's to know, since a window 2 it starts becomes its peer only once its `hello` links it.
impl crate::gtk_host::LinkedWindows for Pair {
    fn open(&self, _win: eitri_core::wm::WinId, name: &WindowName, token: Option<String>) -> Result<(), String> {
        if *name == WindowName::Second {
            let plan = self.window_new(token)?;
            println!("[window] window.new: {plan:?}");
        } else {
            let opened = self.open_module(name.clone(), token)?;
            println!("[window] the window {}: {opened:?}", name.text());
        }
        Ok(())
    }

    /// Ends the window `peer` this window started, as a failed start is ended: SIGTERM, then SIGKILL after the grace,
    /// the application held meanwhile. The link's end then settles whatever was moving. A window that window 1 started
    /// did not start window 1, so it refuses.
    fn end_peer(&self, _win: eitri_core::wm::WinId, peer: PeerId) -> Result<(), String> {
        if self.is_second() {
            return Err("window 1 was not started by this window; close it there".into());
        }
        let module = self.modules.borrow().by_peer(peer).map(|module| module.process.clone());
        let second = self.second.borrow().clone().filter(|_| peer == SECOND_PEER);
        let process = module.or(second).ok_or_else(|| NO_OTHER_WINDOW.to_string())?;
        println!(
            "[window] ending the window in process {}, which is not answering",
            process.pid()
        );
        if let Some(ending) = end_second(&process, END_GRACE) {
            hold_until_finished(ending);
        }
        Ok(())
    }

    fn restore(&self, _win: eitri_core::wm::WinId, name: &WindowName) -> Result<(), String> {
        if *name == WindowName::Second {
            self.restore()?;
            println!("[window] window 2 is started again, as the pair last ended");
        } else {
            self.start_module(name.clone(), None, true)?;
            println!("[window] the window {} is started again", name.text());
        }
        Ok(())
    }

    /// Opens the module window `name`. One a start opens takes no token and is started as one started again is, which
    /// maps without taking the keys; it is left alone when it is open already.
    fn module_window_open(&self, name: &WindowName) -> bool {
        self.modules.borrow().get(name).is_some()
    }

    fn open_module(
        &self,
        _win: eitri_core::wm::WinId,
        name: &WindowName,
        token: Option<String>,
        keep_keys: bool,
    ) -> Result<(), String> {
        if !keep_keys {
            let opened = Pair::open_module(self, name.clone(), token)?;
            println!("[window] the window {}: {opened:?}", name.text());
            return Ok(());
        }
        if *name == WindowName::Second {
            return Err("window 2 is not a module window".to_string());
        }
        if self.modules.borrow().get(name).is_some() {
            println!("[window] the window {} is open already", name.text());
            return Ok(());
        }
        self.start_module(name.clone(), None, true)?;
        println!(
            "[window] the window {} is started, and leaves the keys here",
            name.text()
        );
        Ok(())
    }

    /// Sends `msg` to the window `peer`: a module window's link in window 1, window 2's, or, in a window that window 1
    /// started, window 1's.
    fn send(&self, _win: eitri_core::wm::WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
        let module = self.modules.borrow().by_peer(peer).map(|module| module.link.clone());
        let other = if self.is_second() { HUB_PEER } else { SECOND_PEER };
        let link = module.or_else(|| self.link().filter(|_| peer == other));
        let link = link
            .filter(LinkDriver::linked)
            .ok_or_else(|| NO_OTHER_WINDOW.to_string())?;
        link.send(msg, Vec::new())
            .map_err(|e| format!("the other window could not be told: {e}"))
    }

    fn send_to_hub(&self, win: eitri_core::wm::WinId, msg: Msg) -> Result<(), String> {
        if !self.is_second() {
            return Err("window 1 has no window 1 to ask".to_string());
        }
        crate::gtk_host::LinkedWindows::send(self, win, HUB_PEER, msg)
    }

    /// Hands a tab from the module window `from` to `to`, which is started or raised first. A hand-over that cannot
    /// begin is told to the window that asked, on its own screen; only one nobody can be told of is an `Err`.
    fn hand_over(
        &self,
        _win: eitri_core::wm::WinId,
        from: PeerId,
        agent: &eitri_core::agent_backend::AgentId,
        session: &str,
        to: &WindowName,
        token: Option<String>,
    ) -> Result<(), String> {
        let Some(source) = self.modules.borrow().name_of(from) else {
            return Err("the window that asked for the hand-over is gone".to_string());
        };
        let refused = if self.modules.borrow().hand_over_busy(&source) {
            Some(TAB_ALREADY_MOVING_TEXT.to_string())
        } else if source == *to {
            Some("the tab is in that window already".to_string())
        } else {
            Pair::open_module(self, to.clone(), token).err()
        };
        match refused {
            None => {
                let tab = (agent.clone(), session.to_string());
                self.modules.borrow_mut().begin_hand_over(source, tab, to.clone());
            }
            Some(why) => {
                let msg = Msg::TabKept {
                    session: session.to_string(),
                    why,
                };
                self.modules.borrow().send(vec![(source, msg)]);
            }
        }
        Ok(())
    }

    /// A window 2 with no window 1 starts one, `eitri <root>` from this executable, which links to it once up.
    fn start_editor(&self, _win: eitri_core::wm::WinId) -> Result<(), String> {
        if !self.is_second() {
            return Err("only window 2 starts the editor window".into());
        }
        if self.link().is_some_and(|link| link.is_up()) {
            return Err("the editor window runs already".into());
        }
        if let Some(started) = self.editor_start.borrow().as_ref().filter(|started| started.running()) {
            return Err(format!(
                "the editor window started as process {} has not linked yet",
                started.pid()
            ));
        }
        let exe = self.starts.borrow().exe.clone();
        let flags = relink::EditorFlags {
            legacy: self.legacy_backend.get(),
            clean: self.clean.get(),
        };
        let started = relink::start_editor(&exe, &self.root, flags)
            .map_err(|e| format!("the editor window could not be started: {e}"))?;
        println!("[window] the editor window is started as process {}", started.pid());
        *self.editor_start.borrow_mut() = Some(started);
        Ok(())
    }
}

fn moved_child_watches() -> &'static Mutex<HashSet<u32>> {
    static WATCHED: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    WATCHED.get_or_init(Mutex::default)
}

/// Holds a shell's one watch across repeated departures, releasing it on every way the watcher can end.
struct MovedChildWatch(u32);

impl MovedChildWatch {
    fn claim(pid: u32) -> Option<Self> {
        let mut watched = moved_child_watches().lock().unwrap();
        if !watched.insert(pid) {
            return None;
        }
        Some(Self(pid))
    }
}

impl Drop for MovedChildWatch {
    fn drop(&mut self) {
        moved_child_watches().lock().unwrap().remove(&self.0);
    }
}

/// Watches the shell `pid`, which this process started and whose terminal went to the other window, until it ends,
/// on a thread of its own: `report` gets its start time and its raw wait status (`None` when it could not be read),
/// read by reaping it, which only this process, its parent, can. A shell this process did not start is not watched:
/// its own parent reports it. At most one watcher per shell runs in this process; another call drops its report.
pub fn watch_moved_child(pid: u32, report: impl FnOnce(u64, Option<i32>) + Send + 'static) -> io::Result<()> {
    let Some(watch) = MovedChildWatch::claim(pid) else {
        return Ok(());
    };
    std::thread::Builder::new()
        .name(format!("moved-shell-{pid}"))
        .spawn(move || {
            let _watch = watch;
            if !is_own_child(pid) {
                return;
            }
            // Nothing here reaps it before this does, so its pid cannot name another process yet.
            let opened = agent::child::start_time(pid).and_then(|start| Ok((start, PidFd::open(pid, start)?)));
            let (start, pidfd) = match opened {
                Ok(opened) => opened,
                Err(err) => {
                    eprintln!("[terminal] the shell {pid} that moved cannot be watched: {err}");
                    return;
                }
            };
            while !pidfd.wait_exited(Duration::from_secs(3600)) {}
            let status = match pidfd.try_reap() {
                Ok(Some(code)) => Some(crate::terminal::moving::raw_status(code)),
                Ok(None) | Err(_) => None,
            };
            println!("[terminal] the shell {pid} that moved ended ({status:?})");
            report(start, status);
        })
        .map(drop)
}

/// Whether `pid` is a child of this process, by its parent in `/proc/<pid>/stat` (field 4, read after the last `)`
/// so a command name with spaces or parentheses cannot shift it).
fn is_own_child(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(rest) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
        return false;
    };
    rest.split_whitespace()
        .nth(1)
        .and_then(|ppid| ppid.parse::<u32>().ok())
        .is_some_and(|ppid| ppid == std::process::id())
}

/// Runs `hook` with `why` once, while `pending` says a restored window 2 has neither linked nor failed yet.
fn fail_restore(pending: &std::cell::Cell<bool>, hook: &RefCell<Option<OnRestoreFailed>>, why: &str) -> bool {
    if !pending.replace(false) {
        return false;
    }
    let hook = hook.borrow().clone();
    match hook {
        Some(hook) => hook(why),
        None => eprintln!("[window] window 2 did not come back ({why}) and nothing takes its modules back"),
    }
    true
}

/// The `linked` hook as the window sets it, read when the link is made, for the link to the window `name`, which this
/// window calls `peer`.
fn linked_hook(hooks: &Hooks, name: WindowName, peer: PeerId) -> LinkMade {
    let hooks = hooks.clone();
    Rc::new(move |link| {
        let hook = hooks.borrow().as_ref().map(|h| h.linked.clone());
        if let Some(linked) = hook {
            linked(&name, peer, link);
        }
    })
}

/// Gives `event`, when the message made one, to the controller's queue.
fn post_event(hooks: &Hooks, event: Option<eitri_core::wm::PeerEvent>) {
    let post = hooks.borrow().as_ref().map(|h| h.post.clone());
    if let (Some(event), Some(post)) = (event, post) {
        post(event);
    }
}

/// Takes up again the tabs this window let go that window 1 will never hear of, as nobody else will be given them.
fn take_lost_tabs(tabs: Vec<SavedTab>, holder: &TabHolderSlot) {
    let held = holder.borrow().clone();
    for tab in tabs {
        match &held {
            Some(held) => {
                println!("[link] window 1 will not hear of it; the tab it asked for stays here");
                held.adopt(tab);
            }
            None => eprintln!("[link] a tab window 1 did not hear of has no window to go back to"),
        }
    }
}

/// Takes the module window `peer` out of the registry, for a link that ended, was refused or missed its handshake. A
/// peer is never reused, so a later window of the same name is safe from this.
fn forget_module(modules: &std::rc::Weak<RefCell<ModuleWindows>>, peer: PeerId) -> Option<ModuleLink> {
    modules.upgrade().and_then(|modules| modules.borrow_mut().remove(peer))
}

/// Tells the hook, when one is set, that the module window `name` did not come up, for `why`.
fn module_failed(hook: &RefCell<Option<OnModuleFailed>>, name: &WindowName, why: &str) {
    eprintln!("[window] the window {} did not come up ({why})", name.text());
    let hook = hook.borrow().clone();
    if let Some(hook) = hook {
        hook(name, why);
    }
}

/// How long a started window told to end has before it is killed: enough to close its files, not enough to keep a
/// start that failed around.
pub const END_GRACE: Duration = Duration::from_secs(2);

/// How long a closing window 1 waits for its chat windows' last tabs. A window on its main loop answers at once; one
/// that does not is past the window where a change could have been left unsaid.
pub const LAST_TABS_WITHIN: Duration = Duration::from_millis(300);

/// How often that wait looks.
const LAST_TABS_POLL: Duration = Duration::from_millis(5);

/// A window's process as window 1 holds it. The pidfd names that process and no other: once it has ended, a signal
/// through it reaches nothing, even after its pid is reused. For a window this one started it is opened while the
/// process is still an unreaped child, and the `Child` is waited for on a thread of its own, which reaps it whenever it
/// ends. For a window it came back for ([`SecondProcess::adopted`]) it is opened by pid and start time.
pub struct SecondProcess {
    pid: u32,
    start: u64,
    pidfd: Arc<PidFd>,
    #[cfg_attr(not(test), allow(dead_code))] // The window only logs the reap; the tests wait for it.
    reaped: Arc<Reaped>,
    #[cfg(test)]
    end_requests: std::sync::atomic::AtomicUsize,
}

/// The reaped child's status, once its reaper has it.
#[derive(Default)]
struct Reaped {
    status: Mutex<Option<ExitStatus>>,
    changed: Condvar,
}

/// What ending a window 2 came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// It had ended already: nothing was signalled, and its reaper reaps it.
    AlreadyEnded,
    /// It ended within the grace after SIGTERM.
    Terminated,
    /// It still ran after the grace and was sent SIGKILL.
    Killed,
    /// A signal could not be sent.
    Failed(String),
}

impl SecondProcess {
    /// Takes the child just spawned: reads its start time, opens its pidfd and starts its reaper. On an error the
    /// child is killed, since without those it could not be told from a later holder of its pid, and reaped without
    /// a wait on the caller (see [`reap_without_waiting`]).
    pub fn started(mut child: Child) -> io::Result<SecondProcess> {
        let pid = child.id();
        let named = agent::child::start_time(pid).and_then(|start| Ok((start, PidFd::open(pid, start)?)));
        let (start, pidfd) = match named {
            Ok(named) => named,
            Err(err) => {
                let _ = child.kill();
                reap_without_waiting(child);
                return Err(err);
            }
        };
        let reaped: Arc<Reaped> = Arc::default();
        // Kept here as well until the reaper runs, so a reaper that cannot be started leaves no child behind.
        let slot = Arc::new(Mutex::new(Some(child)));
        let spawned = {
            let slot = slot.clone();
            let reaped = reaped.clone();
            std::thread::Builder::new()
                .name("window-2-reaper".into())
                .spawn(move || {
                    let Some(mut child) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                        return;
                    };
                    match child.wait() {
                        Ok(status) => {
                            println!("[window] the window in process {pid} ended: {status}");
                            *reaped.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
                            reaped.changed.notify_all();
                        }
                        Err(err) => eprintln!("[window] the window in process {pid} could not be waited for: {err}"),
                    }
                })
        };
        if let Err(err) = spawned {
            if let Some(mut child) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = child.kill();
                reap_without_waiting(child);
            }
            return Err(err);
        }
        Ok(SecondProcess {
            pid,
            start,
            pidfd: Arc::new(pidfd),
            reaped,
            #[cfg(test)]
            end_requests: Default::default(),
        })
    }

    /// The orphan at the other end of a re-link: an earlier window 1's child, so there is nothing here to reap, and the
    /// pidfd ends it all the same.
    pub fn adopted(partner: Partner) -> io::Result<SecondProcess> {
        Ok(SecondProcess {
            pid: partner.pid,
            start: partner.start,
            pidfd: Arc::new(PidFd::open(partner.pid, partner.start)?),
            reaped: Arc::default(),
            #[cfg(test)]
            end_requests: Default::default(),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    /// Ends the process on a thread of its own, so no wait falls on the caller: nothing if it has already ended,
    /// else SIGTERM, and SIGKILL once `grace` has passed with it still running. Only this process is signalled,
    /// never its group or anything it started.
    pub fn end(&self, grace: Duration) -> io::Result<JoinHandle<Ending>> {
        #[cfg(test)]
        self.end_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pidfd = self.pidfd.clone();
        let pid = self.pid;
        std::thread::Builder::new()
            .name("window-2-ender".into())
            .spawn(move || {
                let ending = end_process(&pidfd, grace);
                report_ending(pid, &ending);
                ending
            })
    }

    /// Ends a window that was told to close, on a thread of its own: nothing if it goes within `grace`, else SIGTERM,
    /// and SIGKILL once `told` has passed with it still running. `told` has to outlast a healthy close, as SIGTERM's own
    /// bound does, since a window killed while its chats are ending leaves their `claude` and sidecar running. A window
    /// that stopped answering is ended sooner, by its own watchdog, which the SIGTERM wakes. One signal only: a second
    /// forces a close that is under way.
    pub fn end_unless_closed(&self, grace: Duration, told: Duration) -> io::Result<JoinHandle<Ending>> {
        #[cfg(test)]
        self.end_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pidfd = self.pidfd.clone();
        let pid = self.pid;
        std::thread::Builder::new().name("window-ender".into()).spawn(move || {
            let ending = if pidfd.wait_exited(grace) {
                Ending::AlreadyEnded
            } else {
                end_process(&pidfd, told)
            };
            report_ending(pid, &ending);
            ending
        })
    }

    #[cfg(test)]
    pub(crate) fn ending_requests(&self) -> usize {
        self.end_requests.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The reaped status, waiting at most `timeout` for it.
    #[cfg(test)]
    pub(crate) fn wait_reaped(&self, timeout: Duration) -> Option<ExitStatus> {
        let status = self.reaped.status.lock().unwrap();
        let (status, _) = self
            .reaped
            .changed
            .wait_timeout_while(status, timeout, |status| status.is_none())
            .unwrap();
        *status
    }
}

/// SIGTERM, then SIGKILL after `grace`, to the process `pidfd` names; nothing to one that has ended. A process that
/// ends between the check and the signal answers `ESRCH` through its pidfd, and nothing else is reached.
fn end_process(pidfd: &PidFd, grace: Duration) -> Ending {
    end_process_with_wait(pidfd, grace, |within| pidfd.wait_exited(within))
}

fn end_process_with_wait(pidfd: &PidFd, grace: Duration, wait: impl FnOnce(Duration) -> bool) -> Ending {
    if let Some(done) = terminate(pidfd) {
        return done;
    }
    if wait(grace) {
        return Ending::Terminated;
    }
    kill_if_running(pidfd)
}

/// The first half of ending: SIGTERM unless the process has ended. What it came to when nothing is left to do,
/// `None` while SIGKILL may still have to follow. Never waits.
fn terminate(pidfd: &PidFd) -> Option<Ending> {
    if pidfd.exited() {
        return Some(Ending::AlreadyEnded);
    }
    match pidfd.signal(libc::SIGTERM) {
        Ok(()) => None,
        Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Some(Ending::AlreadyEnded),
        Err(err) => Some(Ending::Failed(format!("SIGTERM could not be sent: {err}"))),
    }
}

/// The second half, once the grace has passed: SIGKILL to a process that still runs. Never waits.
fn kill_if_running(pidfd: &PidFd) -> Ending {
    if pidfd.exited() {
        return Ending::Terminated;
    }
    match pidfd.signal(libc::SIGKILL) {
        Ok(()) => Ending::Killed,
        Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Ending::Terminated,
        Err(err) => Ending::Failed(format!("SIGKILL could not be sent: {err}")),
    }
}

/// How often the main loop looks again at a child it reaps without a thread.
const REAP_RETRY: Duration = Duration::from_millis(100);

/// Reaps `child` with no wait on the caller, for when no thread can be started to wait for it: at once if it has
/// ended, else from the main loop once it has. A killed child can still take a while to end (one in uninterruptible
/// I/O ends only when that I/O does), and the caller is the GTK thread.
fn reap_without_waiting(mut child: Child) {
    if reaped_now(&mut child) {
        return;
    }
    gtk4::glib::timeout_add_local(REAP_RETRY, move || {
        if reaped_now(&mut child) {
            gtk4::glib::ControlFlow::Break
        } else {
            gtk4::glib::ControlFlow::Continue
        }
    });
}

/// One look that never waits: whether `child` is reaped now, or can never be (its status cannot be read).
fn reaped_now(child: &mut Child) -> bool {
    !matches!(child.try_wait(), Ok(None))
}

/// Ends the process from the main loop, for when no thread can be started for the grace: SIGTERM now, and a timer
/// that sends SIGKILL once the grace has passed if it still runs. Nothing waits on the caller. The application is
/// held until the timer has run, so closing the last window during the grace does not end this process first.
fn end_from_main_loop(pidfd: Arc<PidFd>, pid: u32, grace: Duration) {
    end_from_main_loop_scheduled(pidfd, pid, grace, |within, run| {
        gtk4::glib::timeout_add_local_once(within, run);
    });
}

fn end_from_main_loop_scheduled(
    pidfd: Arc<PidFd>,
    pid: u32,
    grace: Duration,
    schedule: impl FnOnce(Duration, Box<dyn FnOnce()>),
) {
    if let Some(done) = terminate(&pidfd) {
        report_ending(pid, &done);
        return;
    }
    let hold = hold_application();
    schedule(
        grace,
        Box::new(move || {
            report_ending(pid, &kill_if_running(&pidfd));
            // Released here, after the SIGKILL, rather than whenever GLib frees the finished source.
            drop(hold);
        }),
    );
}

/// A hold on the running application, so it stays in its main loop with no window left. Once window 1's last
/// window closes the process would otherwise exit, and with it the thread or timer that still owes a window 2 its
/// SIGKILL: one that ignores SIGTERM would be left running. `None` when no application runs (the unit tests), where
/// nothing exits under the ending.
fn hold_application() -> Option<gtk4::gio::ApplicationHoldGuard> {
    use gtk4::prelude::*;
    gtk4::gio::Application::default().map(|app| app.hold())
}

/// Holds the application until the ending thread has finished; the thread itself is bounded by its grace.
fn hold_until_finished(ending: JoinHandle<Ending>) {
    let Some(hold) = hold_application() else {
        return;
    };
    release_when_finished(ending, move || drop(hold));
}

/// Runs `release` on the main loop as soon as `ending` has finished. The join happens on GLib's worker pool and wakes
/// the main loop when it returns, so nothing waits on the caller and nothing polls: a window that is already gone lets
/// the process exit at once, not at the next look of a timer (which cost every close in compositor mode about 100
/// ms).
fn release_when_finished<T: Send + 'static>(ending: JoinHandle<T>, release: impl FnOnce() + 'static) {
    let joined = gtk4::gio::spawn_blocking(move || {
        let _ = ending.join();
    });
    gtk4::glib::spawn_future_local(async move {
        let _ = joined.await;
        release();
    });
}

/// Ends `second` with `grace` to go: on a thread when one can be started (the thread, returned), else from the main
/// loop, so a window that ignores SIGTERM is still killed either way.
fn end_second(second: &SecondProcess, grace: Duration) -> Option<JoinHandle<Ending>> {
    match second.end(grace) {
        Ok(ending) => Some(ending),
        Err(err) => {
            // With no thread for the grace, the main loop keeps it.
            eprintln!("[window] the window could not be ended on a thread ({err}); the main loop ends it");
            end_from_main_loop(second.pidfd.clone(), second.pid, grace);
            None
        }
    }
}

/// What a closing module window says last. Going with what it holds kept, that is all of it. Closed by the user, tabs it
/// never opened are what stays; its open tabs he closed.
fn last_tabs(held: &dyn TabHolder, keep: bool) -> Option<SavedTabs> {
    if keep {
        held.tabs_to_keep()
    } else {
        held.unopened().or_else(|| held.tabs_now())
    }
}

/// [`SecondProcess::end_unless_closed`], and with no thread for it, the main loop's end at once: the window was told to
/// close, so SIGTERM asks nothing new of it.
fn end_closing(window: &SecondProcess, grace: Duration, told: Duration) -> Option<JoinHandle<Ending>> {
    match window.end_unless_closed(grace, told) {
        Ok(ending) => Some(ending),
        Err(err) => {
            eprintln!("[window] the window could not be ended on a thread ({err}); the main loop ends it");
            end_from_main_loop(window.pidfd.clone(), window.pid, told);
            None
        }
    }
}

fn report_ending(pid: u32, ending: &Ending) {
    match ending {
        Ending::Failed(why) => eprintln!("[window] the window in process {pid} could not be ended: {why}"),
        done => println!("[window] the window in process {pid}: {done:?}"),
    }
}

/// When the handshake's deadline passes: a window 2 that has not finished it has failed to start, so its link is
/// closed and its process ended with `grace` to go. A linked window is left alone. The ending thread, when one was
/// started.
fn end_if_overdue(
    link: &LinkDriver,
    second: &SecondProcess,
    now: Instant,
    grace: Duration,
) -> Option<JoinHandle<Ending>> {
    if !link.handshake_overdue(now) {
        return None;
    }
    eprintln!(
        "[window] the window in process {} did not finish its handshake; it is ended",
        second.pid
    );
    end_second(second, grace)
}

/// Window 1 starts window 2 ([`start_window`]); a window 2 refused at its hello ends by itself, once it reads the
/// `bye`.
#[allow(clippy::too_many_arguments)]
pub fn start_second(
    starts: &Starts,
    root: &Path,
    token: Option<&str>,
    flags: LinkedFlags,
    lock: Rc<PairLock>,
    linked: LinkMade,
    running: Rc<RefCell<Option<RunningTurns>>>,
    overdue: Option<Rc<dyn Fn()>>,
) -> io::Result<(LinkDriver, Rc<SecondProcess>)> {
    let name = WindowName::Second;
    start_window(starts, root, &name, token, flags, lock, linked, running, overdue, None)
}

/// Window 1: makes the link, starts the window `name` from `starts`' executable (this one's, in a window) with the
/// other end and `flags`, and links when its `hello` matches. `linked` runs once `welcome` is sent. The child is
/// reaped on a thread of its own, and ended if it has not finished the handshake in `starts`' time
/// ([`HANDSHAKE_WITHIN`] in a window); `overdue`, when given, runs then if the link is not linked. `refused`, when
/// given, runs once a `hello` has been refused and the link closed.
#[allow(clippy::too_many_arguments)]
fn start_window(
    starts: &Starts,
    root: &Path,
    name: &WindowName,
    token: Option<&str>,
    flags: LinkedFlags,
    lock: Rc<PairLock>,
    linked: LinkMade,
    running: Rc<RefCell<Option<RunningTurns>>>,
    overdue: Option<Rc<dyn Fn()>>,
    refused: Option<Rc<dyn Fn()>>,
) -> io::Result<(LinkDriver, Rc<SecondProcess>)> {
    let (mine, theirs) = (starts.link_ends)()?;
    let second = Rc::new(SecondProcess::started(spawn_linked(
        &starts.exe,
        theirs,
        root,
        name,
        token,
        flags,
    )?)?);
    let (pid, start) = (second.pid(), second.start());
    let presence = not_yet_confirmed();
    let (handle, inbound) = match LinkHandle::start(mine, presence.clone()) {
        Ok(started) => started,
        Err(err) => {
            // No link means no handshake and no deadline to end it by: a window hung before its toolkit came up
            // would never see its socket close, so it is ended here.
            eprintln!("[window] the link to process {pid} could not be started ({err}); it is ended");
            if let Some(ending) = end_second(&second, END_GRACE) {
                hold_until_finished(ending);
            }
            return Err(err);
        }
    };
    let link = LinkDriver::new(handle, inbound);
    link.set_presence(presence);
    let expected = Expected {
        build: crate::version::fingerprint(),
        name: name.clone(),
        pid,
        start,
    };
    link.on("hello", move |link, msg, _| {
        let running = running.borrow().clone();
        let running = move || running.as_ref().map(|running| running()).unwrap_or_default();
        answer_hello(link, &msg, &expected, &lock, &linked, &running);
        if !link.linked() {
            if let Some(refused) = &refused {
                refused();
            }
        }
    });
    link.watch();
    // A window that never says hello (it hung before its toolkit came up, or died in a way the link does not see)
    // would otherwise leave this link up for good, and it could never be started again; its process, left running,
    // would keep whatever it opened.
    link.set_handshake_deadline(Instant::now() + starts.handshake_within);
    {
        let link = link.clone();
        let second = second.clone();
        (starts.after)(
            starts.handshake_within,
            Box::new(move || {
                // The ending thread is bounded by the grace; the application is held until it has finished.
                if let Some(ending) = end_if_overdue(&link, &second, Instant::now(), END_GRACE) {
                    hold_until_finished(ending);
                }
                if !link.linked() {
                    if let Some(overdue) = &overdue {
                        overdue();
                    }
                }
            }),
        );
    }
    println!("[window] started window {} as process {pid}", name.text());
    Ok((link, second))
}

/// The other window's presence flag before it has said anything: it counts as another window until it says it holds
/// its presence.
fn not_yet_confirmed() -> PresenceFlag {
    PresenceFlag(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)))
}

fn answer_hello(
    link: &LinkDriver,
    msg: &Msg,
    expected: &Expected,
    lock: &PairLock,
    linked: &LinkMade,
    running: &dyn Fn() -> Vec<TurnEdge>,
) {
    if link.linked() {
        eprintln!("[link] a second hello on a linked link; ignored");
        return;
    }
    // What was queued behind a refused hello is still handed over: the link it came on is closed, and stays so.
    if !link.is_up() {
        return;
    }
    let partner = match check_hello(msg, expected) {
        Ok(partner) => partner,
        Err(why) => {
            eprintln!("[link] {why}; the link is closed");
            link.close(&why);
            return;
        }
    };
    let shared = match lock.share() {
        Ok(fd) => fd,
        Err(err) => {
            let why = format!("the pair lock could not be shared ({err})");
            eprintln!("[link] {why}; the link is closed");
            link.close(&why);
            return;
        }
    };
    link.set_partner(partner);
    if let Msg::Hello { running, .. } = msg {
        link.set_partner_running(running.clone());
    }
    if let Err(err) = link.send(welcome(BTreeMap::new(), running()), vec![shared]) {
        let why = format!("welcome could not be sent ({err})");
        eprintln!("[link] {why}; the link is closed");
        link.close(&why);
        return;
    }
    link.set_linked();
    println!("[link] linked to the window in process {}", partner.pid);
    linked(link);
}

/// The window `name`, started by window 1: starts the link on its checked end, says `hello`, and links on `welcome`,
/// adopting the pair lock it carries into `lock`, which the window keeps, so the lock is held for as long as this
/// window lives, after the link ended too, and starting `stamp` on it. `linked` runs once linked.
pub fn link_second(
    sock: OwnedFd,
    name: &WindowName,
    linked: LinkMade,
    lock: Rc<RefCell<Option<PairLock>>>,
    stamp: Rc<StampSlot>,
) -> io::Result<LinkDriver> {
    // SAFETY: neither call has preconditions or can fail.
    let (pid, parent) = unsafe { (libc::getpid(), libc::getppid()) };
    let (pid, parent) = (pid as u32, parent as u32);
    let start = agent::child::start_time(pid)?;
    let presence = not_yet_confirmed();
    let (handle, inbound) = LinkHandle::start(sock, presence.clone())?;
    let link = LinkDriver::new(handle, inbound);
    link.set_presence(presence);
    {
        let lock = lock.clone();
        link.on("welcome", move |link, msg, fds| {
            take_welcome(link, &msg, fds, parent, &lock, &stamp, &linked);
        });
    }
    link.send(hello(crate::version::fingerprint(), name, pid, start), Vec::new())
        .map_err(|e| io::Error::other(e.to_string()))?;
    link.watch();
    Ok(link)
}

fn take_welcome(
    link: &LinkDriver,
    msg: &Msg,
    fds: Vec<OwnedFd>,
    parent: u32,
    lock: &Rc<RefCell<Option<PairLock>>>,
    stamp: &StampSlot,
    linked: &LinkMade,
) {
    if link.linked() {
        eprintln!("[link] a second welcome on a linked link; ignored");
        return;
    }
    let Msg::Welcome { window, running, .. } = msg else {
        return;
    };
    if window != FIRST_WINDOW {
        let why = format!("welcome came from window {window:?}");
        eprintln!("[link] {why}; the link is closed");
        link.close(&why);
        return;
    }
    // The link checked that `welcome` carries exactly one descriptor.
    let Some(fd) = fds.into_iter().next() else { return };
    match PairLock::adopt(fd) {
        Ok(adopted) => {
            stamp.fill(ProjectStamp::start(&adopted, None));
            *lock.borrow_mut() = Some(adopted);
        }
        Err(err) => {
            let why = format!("the pair lock in welcome is not one ({err})");
            eprintln!("[link] {why}; the link is closed");
            link.close(&why);
            return;
        }
    }
    let start = agent::child::start_time(parent).unwrap_or(0);
    link.set_partner(Partner { pid: parent, start });
    link.set_partner_running(running.clone());
    link.set_linked();
    println!("[link] linked to window 1 (process {parent})");
    linked(link);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ide_wiring::ShellPort;
    use crate::linked_window::driver::tests::{pair, pump_until};
    use eitri_core::window_link::link::Inbound;
    use std::ffi::OsString;
    use std::time::Duration;

    fn expected() -> Expected {
        Expected {
            build: "0.4.0+abc".into(),
            name: WindowName::Second,
            pid: 4242,
            start: 99,
        }
    }

    #[test]
    fn the_handshake_refuses_another_build() {
        let ok = hello("0.4.0+abc".into(), &WindowName::Second, 4242, 99);
        assert_eq!(check_hello(&ok, &expected()), Ok(Partner { pid: 4242, start: 99 }));

        let other_build = hello("0.4.1+def".into(), &WindowName::Second, 4242, 99);
        let err = check_hello(&other_build, &expected()).unwrap_err();
        assert!(err.contains("another build"), "{err}");

        let other_child = hello("0.4.0+abc".into(), &WindowName::Second, 4243, 99);
        assert!(check_hello(&other_child, &expected())
            .unwrap_err()
            .contains("not the window started here"));
        let reused_pid = hello("0.4.0+abc".into(), &WindowName::Second, 4242, 100);
        assert!(
            check_hello(&reused_pid, &expected()).is_err(),
            "the same pid started at another time is another process"
        );
        let mut window_3 = hello("0.4.0+abc".into(), &WindowName::Second, 4242, 99);
        if let Msg::Hello { window, .. } = &mut window_3 {
            *window = "3".into();
        }
        assert!(check_hello(&window_3, &expected()).is_err());
        assert!(check_hello(&Msg::Presence { held: true }, &expected()).is_err());
    }

    #[test]
    fn a_refused_hello_is_answered_with_bye_and_the_link_closes() {
        let (driver, theirs, their_inbound) = pair();
        let dir = crate::linked_window::tests::Scratch::new("refused-hello");
        let lock = Rc::new(PairLock::try_take(&dir.0, Path::new("/p")).unwrap().unwrap());
        let linked_calls: Rc<std::cell::Cell<u32>> = Rc::default();
        let linked: LinkMade = {
            let calls = linked_calls.clone();
            Rc::new(move |_| calls.set(calls.get() + 1))
        };
        let exp = expected();
        driver.on("hello", move |link, msg, _| {
            answer_hello(link, &msg, &exp, &lock, &linked, &Vec::new)
        });
        theirs
            .send(hello("0.9.9+zzz".into(), &WindowName::Second, 4242, 99), Vec::new())
            .unwrap();
        assert!(pump_until(&driver, || !driver.is_up()));
        assert_eq!(linked_calls.get(), 0);
        let mut got = Vec::new();
        while let Ok(inbound) = their_inbound.recv_timeout(Duration::from_secs(5)) {
            match inbound {
                Inbound::Msg { msg, fds } => {
                    assert!(fds.is_empty(), "no lock goes to a refused window");
                    got.push(msg);
                }
                Inbound::Closed => break,
                Inbound::Refused(err) => panic!("{err}"),
            }
        }
        assert!(
            matches!(got.as_slice(), [Msg::Bye { why }] if why.contains("another build")),
            "{got:?}"
        );
    }

    #[test]
    fn a_matching_hello_is_welcomed_with_the_pair_lock() {
        let (driver, theirs, their_inbound) = pair();
        let dir = crate::linked_window::tests::Scratch::new("welcome");
        let root = Path::new("/p");
        let lock = Rc::new(PairLock::try_take(&dir.0, root).unwrap().unwrap());
        let linked_calls: Rc<std::cell::Cell<u32>> = Rc::default();
        let linked: LinkMade = {
            let calls = linked_calls.clone();
            Rc::new(move |_| calls.set(calls.get() + 1))
        };
        let exp = expected();
        {
            let lock = lock.clone();
            driver.on("hello", move |link, msg, _| {
                answer_hello(link, &msg, &exp, &lock, &linked, &Vec::new)
            });
        }
        theirs
            .send(hello("0.4.0+abc".into(), &WindowName::Second, 4242, 99), Vec::new())
            .unwrap();
        assert!(pump_until(&driver, || driver.linked()));
        assert_eq!(linked_calls.get(), 1);
        assert_eq!(driver.partner(), Some(Partner { pid: 4242, start: 99 }));
        match their_inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
            Inbound::Msg {
                msg: Msg::Welcome { window, .. },
                fds,
            } => {
                assert_eq!(window, FIRST_WINDOW);
                assert_eq!(fds.len(), 1);
                let adopted = PairLock::adopt(fds.into_iter().next().unwrap()).unwrap();
                drop(lock);
                drop(driver);
                assert!(
                    PairLock::try_take(&dir.0, root).unwrap().is_none(),
                    "the copy window 2 holds keeps the pair locked after window 1's own is gone"
                );
                drop(adopted);
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    /// A stand-in for window 2 that never says hello: `sleep`, through the seam window 1 starts window 2 by.
    /// `ignore_term`: it ignores SIGTERM, as a window hung with that signal blocked would.
    fn stand_in(ignore_term: bool) -> SecondProcess {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        if ignore_term {
            // SAFETY: `signal` is async-signal-safe; an ignored disposition survives `exec`.
            unsafe {
                command.pre_exec(|| {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        SecondProcess::started(command.spawn().unwrap()).unwrap()
    }

    fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
        std::os::unix::process::ExitStatusExt::signal(&status)
    }

    #[test]
    fn a_window_2_that_misses_its_handshake_is_ended_and_reaped() {
        use std::time::Instant;
        // A process that heeds SIGTERM gets a long grace, since ending returns as soon as it has gone: a slow machine
        // must not turn its SIGTERM into a SIGKILL. Only the one that ignores SIGTERM waits out a short grace.
        let grace = Duration::from_secs(10);
        let short_grace = Duration::from_millis(300);
        let start = Instant::now();
        let overdue = start + HANDSHAKE_WITHIN;

        // A window that never said hello: its link is closed, and the process gets SIGTERM.
        let (driver, _theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(overdue);
        let second = stand_in(false);
        assert!(
            end_if_overdue(&driver, &second, overdue - Duration::from_millis(1), grace).is_none(),
            "before the deadline the start is still waited for"
        );
        let ending = end_if_overdue(&driver, &second, overdue, grace).expect("ended once overdue");
        assert_eq!(ending.join().unwrap(), Ending::Terminated);
        assert!(!driver.is_up(), "its link is closed too");
        let status = second.wait_reaped(Duration::from_secs(10)).expect("reaped");
        assert_eq!(signal_of(status), Some(libc::SIGTERM), "{status:?}");
        assert!(
            end_if_overdue(&driver, &second, overdue + HANDSHAKE_WITHIN, grace).is_none(),
            "it is ended once"
        );

        // One that ignores SIGTERM is killed once the grace has passed.
        let (driver, _theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(overdue);
        let stubborn = stand_in(true);
        // `spawn` returns once the child has exec'd, so the disposition is already in place.
        let ending = end_if_overdue(&driver, &stubborn, overdue, short_grace).expect("ended once overdue");
        assert_eq!(ending.join().unwrap(), Ending::Killed);
        let status = stubborn.wait_reaped(Duration::from_secs(10)).expect("reaped");
        assert_eq!(signal_of(status), Some(libc::SIGKILL), "{status:?}");

        // One that already exited is only reaped: nothing is signalled.
        let (driver, _theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(overdue);
        let gone = SecondProcess::started(std::process::Command::new("true").spawn().unwrap()).unwrap();
        let status = gone.wait_reaped(Duration::from_secs(10)).expect("reaped on its own");
        assert_eq!(status.code(), Some(0));
        let ending = end_if_overdue(&driver, &gone, overdue, grace).expect("its link is still closed");
        assert_eq!(ending.join().unwrap(), Ending::AlreadyEnded);

        // A linked window is never signalled, whatever the clock says.
        let (driver, _theirs, _their_inbound) = pair();
        driver.set_handshake_deadline(overdue);
        driver.set_linked();
        let linked = stand_in(false);
        assert!(end_if_overdue(&driver, &linked, overdue + HANDSHAKE_WITHIN, grace).is_none());
        assert!(driver.linked());
        assert_eq!(linked.ending_requests(), 0, "a linked window schedules no ending");
        // The test's own clean-up of its stand-in.
        assert_eq!(linked.end(grace).unwrap().join().unwrap(), Ending::Terminated);
        assert!(linked.wait_reaped(Duration::from_secs(10)).is_some());
    }

    #[test]
    fn without_a_thread_sigkill_still_follows_and_the_child_is_reaped_by_looks_that_never_wait() {
        // The two halves the main loop runs when no thread can be started, the grace's timer between them.
        let stubborn = stand_in(true);
        assert_eq!(
            terminate(&stubborn.pidfd),
            None,
            "SIGTERM is sent, and SIGKILL may still have to follow"
        );
        assert_eq!(kill_if_running(&stubborn.pidfd), Ending::Killed);
        let status = stubborn.wait_reaped(Duration::from_secs(10)).expect("reaped");
        assert_eq!(signal_of(status), Some(libc::SIGKILL), "{status:?}");

        let polite = stand_in(false);
        assert_eq!(terminate(&polite.pidfd), None);
        let status = polite.wait_reaped(Duration::from_secs(10)).expect("reaped");
        assert_eq!(signal_of(status), Some(libc::SIGTERM), "{status:?}");
        assert_eq!(
            kill_if_running(&polite.pidfd),
            Ending::Terminated,
            "a process that has gone is not signalled again"
        );
        assert_eq!(terminate(&polite.pidfd), Some(Ending::AlreadyEnded));

        // A child killed with no reaper thread is reaped by repeated looks, none of which waits.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!reaped_now(&mut child), "a look does not reap a child that still runs");
        child.kill().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !reaped_now(&mut child) {
            assert!(
                std::time::Instant::now() < deadline,
                "the killed child was never reaped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = child.try_wait().unwrap().expect("its status is kept once reaped");
        assert_eq!(signal_of(status), Some(libc::SIGKILL), "{status:?}");
    }

    /// The main loop keeps the hold while the ending thread is parked, then releases it after the join.
    #[test]
    fn a_finished_ending_is_released_without_waiting_for_a_timer() {
        use std::cell::Cell;
        use std::sync::mpsc;
        let context = gtk4::glib::MainContext::new();
        let released = Rc::new(Cell::new(false));
        context
            .with_thread_default(|| {
                let (finish, held) = mpsc::channel();
                let ending = std::thread::spawn(move || {
                    held.recv_timeout(Duration::from_secs(10))
                        .expect("the test releases the ending");
                });
                {
                    let released = released.clone();
                    release_when_finished(ending, move || released.set(true));
                }
                let (marked, marker_seen) = mpsc::channel();
                let marker = gtk4::glib::idle_source_new(None, gtk4::glib::Priority::DEFAULT_IDLE, move || {
                    marked.send(()).unwrap();
                    gtk4::glib::ControlFlow::Break
                });
                marker.attach(Some(&context));
                let deadline = Instant::now() + Duration::from_secs(10);
                while marker_seen.try_recv().is_err() {
                    assert!(Instant::now() < deadline, "the main loop never dispatched the marker");
                    context.iteration(false);
                    std::thread::yield_now();
                }
                assert!(!released.get(), "the hold cannot go before the ending finishes");
                finish.send(()).unwrap();
                while !released.get() {
                    assert!(Instant::now() < deadline, "the finished ending never released its hold");
                    context.iteration(false);
                    std::thread::yield_now();
                }
            })
            .unwrap();
        assert!(released.get());
    }

    /// An application whose last window has closed must not exit while a window 2 still waits for its SIGKILL: `run`
    /// returns only once the ending is done, on the thread and on the main-loop timer alike.
    ///
    /// Ignored because `g_application_run` iterates the default main context, which two tests in two threads would
    /// fight over. Run it alone:
    /// `cargo test -p shell --bin shell linked_window::spawn::tests::the_application_outlives_an_ending_in_its_grace -- --ignored --exact`.
    #[test]
    #[ignore = "runs a real gio main loop on the default main context; must not race the rest of the suite"]
    fn the_application_outlives_an_ending_in_its_grace() {
        let _turn = crate::layout_state::default_context_turn();
        use gtk4::gio;
        use gtk4::prelude::*;
        let grace = Duration::from_secs(400);
        for on_timer in [false, true] {
            let app = gio::Application::new(None, gio::ApplicationFlags::NON_UNIQUE);
            let second = Rc::new(stand_in(true));
            let completed = Rc::new(std::cell::Cell::new(false));
            let (activating, observed) = (second.clone(), completed.clone());
            app.connect_activate(move |_| {
                if on_timer {
                    let observed = observed.clone();
                    end_from_main_loop_scheduled(
                        activating.pidfd.clone(),
                        activating.pid,
                        grace,
                        move |within, run| {
                            assert_eq!(within, grace);
                            gtk4::glib::idle_add_local_once(move || {
                                run();
                                observed.set(true);
                            });
                        },
                    );
                } else {
                    let pidfd = activating.pidfd.clone();
                    let (release, held) = std::sync::mpsc::channel();
                    let (entered, entering) = std::sync::mpsc::channel();
                    let ending = std::thread::spawn(move || {
                        end_process_with_wait(&pidfd, grace, |within| {
                            assert_eq!(within, grace);
                            entered.send(()).unwrap();
                            held.recv_timeout(Duration::from_secs(60))
                                .expect("the application dispatched the release");
                            false
                        })
                    });
                    entering
                        .recv_timeout(Duration::from_secs(60))
                        .expect("inside the grace wait");
                    hold_until_finished(ending);
                    let observed = observed.clone();
                    gtk4::glib::idle_add_local_once(move || {
                        observed.set(true);
                        release.send(()).unwrap();
                    });
                }
            });
            let expired = Rc::new(std::cell::Cell::new(false));
            let expired_in_timer = expired.clone();
            let quitting = app.clone();
            let guard = gtk4::glib::timeout_add_local_once(Duration::from_secs(60), move || {
                expired_in_timer.set(true);
                quitting.quit();
            });
            app.run_with_args(&["shell-ending-hold"]);
            if let Some(source) = gtk4::glib::MainContext::default().find_source_by_id(&guard) {
                source.destroy();
            }
            assert!(!expired.get(), "the application never released its hold");
            assert!(completed.get(), "the application returned before the grace boundary");
            let status = second.wait_reaped(Duration::from_secs(60)).expect("reaped");
            assert_eq!(signal_of(status), Some(libc::SIGKILL));
        }
    }

    #[test]
    fn only_a_window_with_the_lock_starts_a_stamp() {
        let dir = crate::linked_window::tests::Scratch::new("stamp-roles");
        let root = Path::new("/home/someone/project");
        let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
        let path = dir.0.join(eitri_core::window_link::pair::pair_lock_name(root));

        for taken in [Ok(None), Err(io::Error::other("no state directory"))] {
            let pair = Pair::full(taken, root, false, false);
            pair.start_stamp(Some(root.to_path_buf()));
            assert!(
                !pair.stamp().started(),
                "an unpaired or lone window holds no descriptor"
            );
        }

        let adopted = PairLock::adopt(lock.share().unwrap()).unwrap();
        let pair = Pair::new(WindowRole::Main { link: None }, Some(adopted), root);
        pair.start_stamp(None);
        assert!(pair.stamp().started());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"",
            "only the window that took the lock names the root"
        );
    }

    /// A chat window's lock comes in `welcome`, after its window may already have taken activation.
    #[test]
    fn a_window_that_adopts_its_lock_in_welcome_stamps_its_activations() {
        let dir = crate::linked_window::tests::Scratch::new("stamp-welcome");
        let root = Path::new("/home/someone/project");
        let lock = PairLock::try_take(&dir.0, root).unwrap().unwrap();
        let path = dir.0.join(eitri_core::window_link::pair::pair_lock_name(root));
        let (one, two) = fdpass::seqpacket_pair().unwrap();
        let pair = Pair::module(two, WindowName::Chat(2), root).unwrap();
        let at = std::time::SystemTime::now() + Duration::from_secs(3600);
        pair.stamp().activated(at);
        assert!(!pair.stamp().started());

        let (handle, _inbound) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
        handle
            .send(welcome(Default::default(), Vec::new()), vec![lock.share().unwrap()])
            .unwrap();
        let link = pair.link().unwrap();
        assert!(pump_until(&link, || link.linked()));
        assert!(pair.stamp().started());
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::fs::metadata(&path).unwrap().modified().unwrap() < at - Duration::from_secs(1) {
            assert!(std::time::Instant::now() < deadline, "the activation never landed");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    /// A link to no nvim yet: a beside pair for a test that never polls it.
    pub(crate) fn idle_beside_link(id: &str) -> Rc<eitri_core::companion::link::CompanionLink> {
        Rc::new(eitri_core::companion::link::CompanionLink::new(
            None,
            eitri_core::companion::Sockets::default(),
            eitri_core::companion::InstallKind::Page { id: id.into() },
        ))
    }

    /// A window beside nvim belongs to no pair: none of the verbs that reach another window of a group can act, and
    /// each says so rather than doing something else.
    #[test]
    fn a_beside_pair_refuses_start_editor_send_to_hub_send_and_focus_other() {
        use crate::gtk_host::LinkedWindows;
        use eitri_core::wm::WinId;
        let pair = Pair::beside(
            WindowName::Page("notes".into()),
            Path::new("/p"),
            idle_beside_link("notes"),
        );
        assert!(pair.is_beside() && !pair.is_second() && !pair.is_module());
        assert_eq!(
            LinkedWindows::start_editor(&*pair, WinId::MAIN),
            Err("only window 2 starts the editor window".to_string())
        );
        assert_eq!(
            LinkedWindows::send_to_hub(&*pair, WinId::MAIN, Msg::WindowFocused {}),
            Err("window 1 has no window 1 to ask".to_string())
        );
        for peer in [HUB_PEER, SECOND_PEER] {
            assert_eq!(
                LinkedWindows::send(&*pair, WinId::MAIN, peer, Msg::WindowFocused {}),
                Err(NO_OTHER_WINDOW.to_string()),
                "{peer:?}"
            );
        }
        assert_eq!(pair.window_other(None), Err(NO_OTHER_WINDOW.to_string()));
        assert_eq!(
            pair.window_new(None),
            Err(crate::linked_window::BESIDE_REFUSAL.to_string())
        );
        assert_eq!(
            pair.open_module(WindowName::Chat(2), None).map(|_| ()),
            Err(MODULE_WINDOWS_ARE_WINDOW_1S.to_string())
        );
        assert!(pair.link().is_none(), "no link to a window 1");
        assert!(!pair.holds_pair() && !pair.keeps_pair_file());
    }

    #[test]
    fn window_other_without_a_linked_window_is_refused() {
        let pair = Pair::new(WindowRole::Unpaired, None, Path::new("/p"));
        assert_eq!(pair.window_other(None), Err(NO_OTHER_WINDOW.to_string()));
        assert_eq!(
            pair.window_new(None),
            Err(crate::linked_window::UNPAIRED_REFUSAL.to_string())
        );
        let alone = Pair::new(WindowRole::Alone, None, Path::new("/p"));
        assert_eq!(
            alone.window_new(None),
            Err(crate::linked_window::ALONE_REFUSAL.to_string())
        );
    }

    #[test]
    fn window_other_in_a_linked_window_asks_the_other_window_to_take_the_keys() {
        let (driver, _theirs, their_inbound) = pair();
        driver.set_linked();
        let pair = Pair::new(WindowRole::Second { link: driver }, None, Path::new("/p"));
        assert_eq!(pair.window_new(Some("tok".into())), Ok(WindowNew::Raise));
        pair.window_other(None).unwrap();
        let mut got = Vec::new();
        while got.len() < 2 {
            match their_inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
                Inbound::Msg { msg, .. } => got.push(msg),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            got,
            vec![
                Msg::Focus {
                    target: FocusTarget::Window,
                    token: Some("tok".into())
                },
                Msg::Focus {
                    target: FocusTarget::Window,
                    token: None
                },
            ]
        );
    }
    #[test]
    fn the_pairs_focus_handler_posts_every_target() {
        use eitri_core::window_link::Side;
        use eitri_core::wm::{PeerEvent, PeerId};
        for second in [false, true] {
            let (driver, theirs, _inbound) = pair();
            let role = if second {
                WindowRole::Second { link: driver.clone() }
            } else {
                WindowRole::Main {
                    link: Some(driver.clone()),
                }
            };
            let pair = Pair::new(role, None, Path::new("/p"));
            let posted = Rc::new(RefCell::new(Vec::new()));
            let saved = posted.clone();
            pair.set_hooks(PairHooks {
                linked: Rc::new(|_, _, _| {}),
                post: Rc::new(move |event| saved.borrow_mut().push(event)),
                ended: Rc::new(|_, _, _, _| {}),
            });
            let peer = PeerId(if second { 1 } else { 2 });
            let targets = [
                FocusTarget::Window,
                FocusTarget::Module { id: "editor".into() },
                FocusTarget::Edge { side: Side::Left },
                FocusTarget::Edge { side: Side::Right },
                FocusTarget::Edge { side: Side::Up },
                FocusTarget::Edge { side: Side::Down },
            ];
            for target in targets {
                let count = posted.borrow().len();
                theirs
                    .send(
                        Msg::Focus {
                            target: target.clone(),
                            token: Some("keys".into()),
                        },
                        Vec::new(),
                    )
                    .unwrap();
                assert!(pump_until(&driver, || posted.borrow().len() > count));
                assert_eq!(
                    posted.borrow().last(),
                    Some(&PeerEvent::FocusIntent {
                        peer,
                        target,
                        token: Some("keys".into())
                    })
                );
            }
            theirs
                .send(
                    Msg::Away {
                        ids: vec!["terminal#2".into()],
                    },
                    Vec::new(),
                )
                .unwrap();
            theirs
                .send(
                    Msg::Monitor {
                        connector: "DP-1".into(),
                        description: "Left".into(),
                    },
                    Vec::new(),
                )
                .unwrap();
            assert!(pump_until(&driver, || posted.borrow().len() == 8));
            assert_eq!(
                &posted.borrow()[6..],
                &[
                    PeerEvent::Held {
                        peer,
                        ids: vec![eitri_core::layout::ModuleId::parse("terminal#2").unwrap()]
                    },
                    PeerEvent::Monitor {
                        peer,
                        connector: "DP-1".into()
                    }
                ]
            );
        }
    }

    #[test]
    fn a_shell_that_moves_again_is_watched_by_one_thread_only() {
        use std::sync::mpsc::{channel, RecvTimeoutError};

        struct KillOnDrop(Child);

        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let mut child = KillOnDrop(std::process::Command::new("sleep").arg("30").spawn().unwrap());
        let pid = child.0.id();
        let expected_start = agent::child::start_time(pid).unwrap();
        let (first, first_reported) = channel();
        watch_moved_child(pid, move |start, status| {
            let _ = first.send((start, status));
        })
        .unwrap();
        let (second, second_reported) = channel();
        watch_moved_child(pid, move |start, status| {
            let _ = second.send((start, status));
        })
        .unwrap();
        // A duplicate drops its callback while the shell still lives, before any reaping can race it.
        let duplicate = second_reported.recv_timeout(Duration::from_secs(5));
        child.0.kill().unwrap();
        let (start, status) = first_reported.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(start, expected_start);
        assert!(status.is_some());
        assert_eq!(
            first_reported.recv_timeout(Duration::from_secs(20)),
            Err(RecvTimeoutError::Disconnected)
        );
        assert_eq!(duplicate, Err(RecvTimeoutError::Disconnected));

        let mut child = KillOnDrop(std::process::Command::new("sleep").arg("30").spawn().unwrap());
        let pid = child.0.id();
        let expected_start = agent::child::start_time(pid).unwrap();
        let (third, third_reported) = channel();
        watch_moved_child(pid, move |start, status| {
            let _ = third.send((start, status));
        })
        .unwrap();
        child.0.kill().unwrap();
        let (start, status) = third_reported.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(start, expected_start);
        assert!(status.is_some());
        assert_eq!(
            third_reported.recv_timeout(Duration::from_secs(20)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn the_spawner_reports_the_exit_of_a_moved_shell() {
        use eitri_terminal::{PtySize, SessionCommand, SessionConfig, SpawnSpec, TerminalColors, TerminalSession};
        use std::os::unix::process::ExitStatusExt;
        let mut spawn = SpawnSpec::shell("/bin/sh".into(), ".".into());
        spawn.args = vec!["-c".into(), "read line; exit 7".into()];
        let session = TerminalSession::spawn(
            SessionConfig {
                spawn,
                size: PtySize {
                    cols: 80,
                    rows: 24,
                    cell_width_px: 8,
                    cell_height_px: 16,
                },
                colors: TerminalColors::default(),
                focused: false,
                tap: None,
            },
            || {},
        )
        .unwrap();
        let pid = session.pid();
        let (answer, answered) = std::sync::mpsc::channel();
        session.send(SessionCommand::Detach(answer));
        let detached = answered
            .recv_timeout(Duration::from_secs(20))
            .unwrap()
            .expect("the shell freezes");
        // The other window holds it now: this session lets the shell go, and only the watch is left to reap it.
        session.send(SessionCommand::Committed);
        drop(session);
        let (report, reported) = std::sync::mpsc::channel();
        watch_moved_child(pid, move |start, status| {
            let _ = report.send((start, status));
        })
        .unwrap();
        // The shell ends when its line comes, written through the PTY's controller side the move carried.
        let mut master = std::fs::File::from(detached.master);
        std::io::Write::write_all(&mut master, b"go\n").unwrap();
        let (start, status) = reported
            .recv_timeout(Duration::from_secs(20))
            .expect("its end is reported");
        assert_eq!(start, detached.start, "named by the start time the record carried");
        let status = ExitStatus::from_raw(status.expect("a status this process read by reaping it"));
        assert_eq!(status.code(), Some(7));

        // A process this one did not start is its own parent's to report.
        let (report, reported) = std::sync::mpsc::channel::<(u64, Option<i32>)>();
        watch_moved_child(1, move |start, status| {
            let _ = report.send((start, status));
        })
        .unwrap();
        assert!(
            matches!(
                reported.recv_timeout(Duration::from_secs(60)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "the watcher finished without reporting"
        );
    }

    #[test]
    fn a_focus_for_a_module_from_the_other_window_reaches_the_controller() {
        use eitri_core::wm::fake::Logged;
        use eitri_core::wm::{ActivationToken, HostOp, WinId};
        let (wm, host) = crate::linked_window::driver::tests::controller();
        let (driver, theirs, _inbound) = pair();
        driver.attach_controller(
            WinId::MAIN,
            eitri_core::wm::PeerId(2),
            eitri_core::window_link::names::WindowName::Second,
            &wm,
        );
        let pair = Pair::new(
            WindowRole::Main {
                link: Some(driver.clone()),
            },
            None,
            Path::new("/p"),
        );
        let posted = Rc::downgrade(&wm);
        pair.set_hooks(PairHooks {
            linked: Rc::new(|_, _, _| {}),
            post: Rc::new(move |event| {
                if let Some(wm) = posted.upgrade() {
                    wm.post(eitri_core::wm::HostEvent::Peer(event));
                }
            }),
            ended: Rc::new(|_, _, _, _| {}),
        });
        theirs
            .send(
                Msg::Focus {
                    target: FocusTarget::Module { id: "editor".into() },
                    token: Some("keys".into()),
                },
                Vec::new(),
            )
            .unwrap();
        let presented = || {
            host.log().iter().any(|logged| {
                matches!(
                    logged,
                    Logged::Op(HostOp::Present { activation: Some(ActivationToken(token)), .. }, _) if token == "keys"
                )
            })
        };
        assert!(
            pump_until(&driver, presented),
            "the controller brings the window forward with the other window's token: {:#?}",
            host.log()
        );
        assert!(
            host.log().iter().any(|logged| matches!(
                logged,
                Logged::Op(HostOp::Focus { to: eitri_core::wm::FocusTo::Module(m), .. }, _)
                    if *m == eitri_core::layout::ModuleId::editor()
            )),
            "and gives the module the keys"
        );
    }

    /// A window 1 holding a pair, whose window 2 is the stand-in `script` (a shell script ignoring its arguments),
    /// with `within` for its handshake and the handshake's timer held for the test to fire. What the restore's
    /// take-back and the window's own `ended` hook were called with is recorded.
    struct Restoring {
        pair: Rc<Pair>,
        timers: Rc<RefCell<Vec<Box<dyn FnOnce()>>>>,
        failed: Rc<RefCell<Vec<String>>>,
        ended: Rc<RefCell<Vec<LinkEnd>>>,
        report: PathBuf,
        _dir: crate::linked_window::tests::Scratch,
    }

    fn restoring(label: &str, script: &str, within: Duration) -> Restoring {
        let dir = crate::linked_window::tests::Scratch::new(label);
        let report = dir.1.join("pid");
        let exe = dir.1.join("window-2-stand-in");
        script_fixture::write_executable_script(
            &exe,
            format!("#!/bin/sh\necho $$ > '{}'\n{script}\n", report.display()),
            0o700,
        )
        .unwrap();
        let lock = PairLock::try_take(&dir.0, Path::new("/p")).unwrap().unwrap();
        let pair = Pair::full(Ok(Some(lock)), &dir.1, false, false);
        let timers: Rc<RefCell<Vec<Box<dyn FnOnce()>>>> = Rc::default();
        let after: After = {
            let timers = timers.clone();
            Rc::new(move |_, run| timers.borrow_mut().push(run))
        };
        pair.set_starts(Starts {
            exe,
            handshake_within: within,
            after,
            ..Starts::default()
        });
        let failed: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let failed = failed.clone();
            pair.on_restore_failed(Rc::new(move |why: &str| failed.borrow_mut().push(why.to_string())));
        }
        let ended: Rc<RefCell<Vec<LinkEnd>>> = Rc::default();
        {
            let ended = ended.clone();
            pair.set_hooks(PairHooks {
                linked: Rc::new(|_, _, _| {}),
                post: Rc::new(|_| {}),
                ended: Rc::new(move |_, _, _, end| ended.borrow_mut().push(end.clone())),
            });
        }
        Restoring {
            pair,
            timers,
            failed,
            ended,
            report,
            _dir: dir,
        }
    }

    impl Restoring {
        /// The stand-in's pid, once it has written it.
        fn stand_in_pid(&self) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(pid) = std::fs::read_to_string(&self.report)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    return pid;
                }
                assert!(Instant::now() < deadline, "the stand-in never ran");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn fire_timers(&self) {
            let timers: Vec<_> = self.timers.borrow_mut().drain(..).collect();
            for timer in timers {
                timer();
            }
        }
    }

    /// Whether `pid` has been reaped: its `/proc` entry goes only then (a zombie keeps it).
    fn reaped(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_err()
    }

    #[test]
    fn a_restore_whose_start_fails_takes_window_2s_part_back_once() {
        let restoring = restoring("restore-start-fails", "exit 0", HANDSHAKE_WITHIN);
        let mut starts = restoring.pair.starts.borrow().clone();
        starts.exe = restoring.report.with_file_name("no-such-executable");
        restoring.pair.set_starts(starts);
        let err = restoring.pair.restore().unwrap_err();
        assert!(err.contains("could not be started"), "{err}");
        assert_eq!(restoring.failed.borrow().len(), 1, "taken back at once");
        assert!(restoring.failed.borrow()[0].contains("could not be started"));
        assert!(
            !restoring.pair.fail_restore("again"),
            "a restore that failed is not failed twice"
        );
        assert_eq!(restoring.failed.borrow().len(), 1);
    }

    #[test]
    fn a_restored_window_2_that_never_says_hello_is_taken_back_once_its_handshake_is_overdue() {
        let restoring = restoring("restore-overdue", "exec sleep 30", Duration::ZERO);
        restoring.pair.restore().unwrap();
        let pid = restoring.stand_in_pid();
        assert!(
            restoring.failed.borrow().is_empty(),
            "nothing is taken back before the deadline"
        );
        restoring.fire_timers();
        assert_eq!(
            *restoring.failed.borrow(),
            ["window 2 did not finish its handshake"],
            "the overdue handshake takes window 2's part back"
        );
        assert!(
            restoring.ended.borrow().is_empty(),
            "the window's own end of a link does not run for it"
        );
        assert!(!restoring.pair.fail_restore("again"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reaped(pid) {
            assert!(Instant::now() < deadline, "the stand-in was not ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_restored_window_2_that_ends_before_hello_is_taken_back_and_window_1_keeps_the_pair_file() {
        let restoring = restoring("restore-ends-early", "exit 0", HANDSHAKE_WITHIN);
        let dir = restoring.report.parent().unwrap().join("pair-files");
        restoring
            .pair
            .set_pair_writer(PairWriter::new(&dir, Path::new("/p"), Default::default(), true));
        restoring.pair.restore().unwrap();
        let link = restoring.pair.link().unwrap();
        assert!(pump_until(&link, || link.ended().is_some()));
        assert_eq!(*restoring.failed.borrow(), ["window 2 ended before it linked"]);
        assert!(
            restoring.ended.borrow().is_empty(),
            "the take-back runs instead of the window's own end of a link"
        );
        // The handshake's timer still fires later, and finds the restore handled.
        restoring.fire_timers();
        assert_eq!(restoring.failed.borrow().len(), 1);
        // A window 2 that never linked never held the pair file: window 1 goes on writing it.
        restoring.pair.window2_link_ended(&link, &LinkEnd::Eof);
        let mut writing = false;
        restoring.pair.with_pair_writer(|writer| writing = writer.writing());
        assert!(writing, "window 1 still writes the pair file");
    }

    #[test]
    fn only_a_window_2_that_linked_and_ended_without_a_goodbye_keeps_the_pair_file() {
        let dir = crate::linked_window::tests::Scratch::new("link-ended-files");
        let lock = PairLock::try_take(&dir.0, Path::new("/p")).unwrap().unwrap();
        let pair = Pair::full(Ok(Some(lock)), &dir.1, false, false);
        let writing = |pair: &Pair| {
            let mut writing = false;
            pair.with_pair_writer(|writer| writing = writer.writing());
            writing
        };
        let (never_linked, _theirs, _their_inbound) = crate::linked_window::driver::tests::pair();
        pair.set_pair_writer(PairWriter::new(&dir.0, Path::new("/p"), Default::default(), true));
        pair.window2_link_ended(&never_linked, &LinkEnd::Eof);
        assert!(
            writing(&pair),
            "a window 2 that never linked leaves the file to window 1"
        );

        let (linked, _theirs, _their_inbound) = crate::linked_window::driver::tests::pair();
        linked.set_partner(Partner { pid: 4242, start: 99 });
        pair.window2_link_ended(&linked, &LinkEnd::Eof);
        assert!(
            !writing(&pair),
            "a linked window 2 may run on, orphaned, and write it itself"
        );
    }

    #[test]
    fn run_action_runs_the_named_action_in_the_module_window_and_drops_a_bad_name() {
        use eitri_core::keymap::{Action, TabAction};
        let panels = vec!["notes".to_string()];
        assert_eq!(
            crate::linked_window::registry::peer_action("tab.select", &[("n".into(), 3.into())], &panels),
            Some(Action::Tab(TabAction::Select(3)))
        );
        for (name, opts) in [
            ("no.such.action", vec![]),
            ("tab.select", vec![("n".to_string(), serde_json::Value::from("three"))]),
            ("module.missing", vec![]),
        ] {
            assert_eq!(
                crate::linked_window::registry::peer_action(name, &opts, &panels),
                None,
                "{name}"
            );
        }
        // Over the link: the module window's hook hears the name and options, and a window without a hook drops it.
        let dir = crate::linked_window::tests::Scratch::new("run-action");
        let root = Path::new("/p");
        let lock = eitri_core::window_link::pair::PairLock::try_take(&dir.0, root)
            .unwrap()
            .unwrap();
        let (one, two) = eitri_core::window_link::fdpass::seqpacket_pair().unwrap();
        let pair = Pair::module(two, WindowName::Chat(2), root).unwrap();
        type Heard = Vec<(String, Vec<(String, serde_json::Value)>)>;
        let heard: Rc<RefCell<Heard>> = Rc::default();
        {
            let heard = heard.clone();
            pair.set_peer_action(Rc::new(move |name, opts| heard.borrow_mut().push((name, opts))));
        }
        let (handle, _inbound) = eitri_core::window_link::link::LinkHandle::start(
            one,
            eitri_core::window_link::link::PresenceFlag::default(),
        )
        .unwrap();
        handle
            .send(welcome(Default::default(), Vec::new()), vec![lock.share().unwrap()])
            .unwrap();
        let link = pair.link().unwrap();
        assert!(pump_until(&link, || link.linked()));
        let run = Msg::RunAction {
            name: "tab.select".into(),
            opts: vec![("n".into(), 3.into())],
        };
        handle.send(run, Vec::new()).unwrap();
        assert!(pump_until(&link, || !heard.borrow().is_empty()));
        assert_eq!(
            *heard.borrow(),
            vec![(
                "tab.select".to_string(),
                vec![("n".to_string(), serde_json::Value::from(3))]
            )]
        );
    }

    const PORT: u16 = 23456;

    fn port_env(port: u16) -> Vec<(OsString, OsString)> {
        vec![(OsString::from("CLAUDE_CODE_SSE_PORT"), OsString::from(port.to_string()))]
    }

    /// A window 1 and a window 2 on one link that is up, as a handshake or a re-link leaves them. Neither has hooks:
    /// what is held here is the pair's own part.
    fn two_windows() -> (Rc<Pair>, Rc<Pair>) {
        let (one, two) = fdpass::seqpacket_pair().unwrap();
        let (one, one_in) = LinkHandle::start(one, PresenceFlag::default()).unwrap();
        let (two, two_in) = LinkHandle::start(two, PresenceFlag::default()).unwrap();
        let (one, two) = (LinkDriver::new(one, one_in), LinkDriver::new(two, two_in));
        one.set_linked();
        two.set_linked();
        (
            Pair::new(WindowRole::Main { link: Some(one) }, None, Path::new("/p")),
            Pair::new(WindowRole::Second { link: two }, None, Path::new("/p")),
        )
    }

    /// Pumps window 2 until its next shell's port reads `want`.
    fn wait_for_port(two: &Pair, want: Option<u16>) {
        let link = two.link().unwrap();
        assert!(pump_until(&link, || two.ide_port() == want), "{:?}", two.ide_port());
    }

    #[test]
    fn a_window_2_restored_with_a_terminal_shown_has_the_port_on_its_first_shell() {
        let (_window1, two) = fdpass::seqpacket_pair().unwrap();
        let second = Pair::second(two, Path::new("/p")).unwrap();
        second.set_ide_port(Some(PORT));
        assert!(!second.link().unwrap().linked(), "no welcome has come");
        assert_eq!(ShellPort::shared(second.ide_port_cell()).env(), port_env(PORT));
    }

    #[test]
    fn a_relink_brings_the_new_port() {
        let (window1, window2) = two_windows();
        let shell = ShellPort::shared(window2.ide_port_cell());
        assert_eq!(shell.env(), Vec::new(), "welcome said nothing of a port");

        window1.set_ide_port(Some(PORT));
        window1.say_ide_port(&window1.link().unwrap());

        wait_for_port(&window2, Some(PORT));
        assert_eq!(shell.env(), port_env(PORT));
    }

    #[test]
    fn a_shell_started_while_orphaned_has_no_port() {
        let (window1, window2) = two_windows();
        window2.set_ide_port(Some(PORT));
        let shell = ShellPort::shared(window2.ide_port_cell());
        let link = window2.link().unwrap();

        window1.link().unwrap().close("window 1 closed");

        assert!(pump_until(&link, || link.ended().is_some()));
        assert_eq!(shell.env(), Vec::new());
    }

    #[test]
    fn a_window_1_keeps_its_port_when_its_window_2_goes() {
        let (window1, window2) = two_windows();
        window1.set_ide_port(Some(PORT));
        let link = window1.link().unwrap();
        window2.link().unwrap().close("window 2 closed");
        assert!(pump_until(&link, || link.ended().is_some()));
        assert_eq!(window1.ide_port(), Some(PORT));
    }

    #[test]
    fn a_port_message_changes_no_running_shell() {
        let (window1, window2) = two_windows();
        window2.set_ide_port(Some(PORT));
        let shell = ShellPort::shared(window2.ide_port_cell());
        let running = shell.env();

        window1.ide_port_changed(Some(PORT + 1));
        wait_for_port(&window2, Some(PORT + 1));

        assert_eq!(running, port_env(PORT), "a shell keeps what it started with");
        assert_eq!(shell.env(), port_env(PORT + 1), "the next one has the new port");
    }

    #[test]
    fn a_window_1_with_the_key_off_sends_no_port() {
        let (window1, window2) = two_windows();
        let heard: Rc<std::cell::Cell<u32>> = Rc::default();
        let done: Rc<std::cell::Cell<bool>> = Rc::default();
        let link = window2.link().unwrap();
        {
            let heard = heard.clone();
            link.on("ide_port", move |_, _, _| heard.set(heard.get() + 1));
            let done = done.clone();
            link.on("away", move |_, _, _| done.set(true));
        }

        window1.say_ide_port(&window1.link().unwrap());

        // The link keeps order: a port sent above would arrive first.
        window1
            .link()
            .unwrap()
            .send(Msg::Away { ids: Vec::new() }, Vec::new())
            .unwrap();
        assert!(pump_until(&link, || done.get()));
        assert_eq!(heard.get(), 0);
    }

    #[test]
    fn a_late_attach_reaches_a_linked_window_2() {
        let (window1, window2) = two_windows();
        window1.ide_port_changed(Some(PORT));
        assert_eq!(window1.ide_port(), Some(PORT), "window 1's own shells have it too");
        wait_for_port(&window2, Some(PORT));
    }

    #[test]
    fn a_lost_listener_clears_window_2s_port() {
        let (window1, window2) = two_windows();
        window1.ide_port_changed(Some(PORT));
        wait_for_port(&window2, Some(PORT));

        window1.ide_port_changed(None);

        assert_eq!(window1.ide_port(), None);
        wait_for_port(&window2, None);
    }

    #[test]
    fn a_link_in_its_handshake_hears_no_port_until_it_is_up() {
        let (one, _theirs, their_inbound) = pair();
        let window1 = Pair::new(WindowRole::Main { link: Some(one) }, None, Path::new("/p"));

        window1.ide_port_changed(Some(PORT));
        window1
            .link()
            .unwrap()
            .send(Msg::Away { ids: Vec::new() }, Vec::new())
            .unwrap();
        assert!(
            matches!(
                their_inbound.recv_timeout(Duration::from_secs(60)),
                Ok(Inbound::Msg {
                    msg: Msg::Away { .. },
                    ..
                })
            ),
            "nothing goes ahead of welcome or the queue's marker"
        );

        let link = window1.link().unwrap();
        link.set_linked();
        window1.say_ide_port(&link);
        assert!(matches!(
            their_inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::Msg {
                msg: Msg::IdePort { port: Some(PORT) },
                ..
            })
        ));
    }

    #[test]
    fn a_listener_lost_during_the_handshake_clears_the_port_on_window_2s_command_line() {
        let (window1, window2) = two_windows();
        window2.set_ide_port(Some(PORT));
        window1.set_ide_port(Some(PORT));
        assert_eq!(window1.second_flags(false).ide_port, Some(PORT));

        // The link was not up when this happened, so nothing was said.
        window1.set_ide_port(None);
        window1.say_ide_port(&window1.link().unwrap());

        wait_for_port(&window2, None);
    }

    #[test]
    fn only_window_2_is_started_with_the_port() {
        let (window1, _window2) = two_windows();
        window1.set_ide_port(Some(PORT));
        assert_eq!(window1.second_flags(true).ide_port, Some(PORT));
        assert!(window1.second_flags(true).restore);
        assert_eq!(window1.flags(false).ide_port, None, "a module window has no terminal");
    }

    #[test]
    fn a_restored_window_2_is_started_with_the_port_as_it_stands() {
        let restoring = restoring(
            "restore-port",
            r#"echo "$*" > "$0.args.tmp"; mv "$0.args.tmp" "$0.args""#,
            HANDSHAKE_WITHIN,
        );
        let args = restoring.report.with_file_name("window-2-stand-in.args");
        restoring.pair.set_ide_port(Some(PORT));
        restoring.pair.restore().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let line = loop {
            if let Ok(line) = std::fs::read_to_string(&args) {
                break line;
            }
            assert!(Instant::now() < deadline, "the stand-in never ran");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            line.starts_with(&format!("linked-window --restore --ide-port {PORT} 3 2 ")),
            "{line}"
        );
    }
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
