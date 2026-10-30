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

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
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
use eitri_core::window_link::pair::PairLock;
use eitri_core::window_link::start::{spawn_second, SECOND_WINDOW};
use eitri_core::window_link::{FocusTarget, Msg, Summary, TurnEdge};

use super::driver::{LinkDriver, Partner, HANDSHAKE_WITHIN};
use super::pair_files::PairWriter;
use super::relink::{self, HeldNow, OrphanSocket, Relinked};
use super::{LinkEnd, WindowNew, WindowRole, NO_OTHER_WINDOW};

/// Window 1's name on the link.
pub const FIRST_WINDOW: &str = "1";

/// The `hello` window 1 accepts: from the child it started, of its own build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub build: String,
    pub pid: u32,
    pub start: u64,
}

/// Checks window 2's `hello` against what window 1 started; the partner it names, or why it is refused.
pub fn check_hello(msg: &Msg, expected: &Expected) -> Result<Partner, String> {
    let Msg::Hello {
        build,
        window,
        pid,
        start,
        ..
    } = msg
    else {
        return Err(format!(
            "the second window's first message was `{}`, not hello",
            msg.kind()
        ));
    };
    if *build != expected.build {
        return Err(format!(
            "the second window is another build ({build}, this one is {})",
            expected.build
        ));
    }
    if window != SECOND_WINDOW {
        return Err(format!("the second window calls itself window {window:?}"));
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

/// Window 2's `hello`: its build, its process, and nothing confirmed yet about its own presence (the partner
/// counts it as another window until it says so). It is said before the window's panel exists, so no turn of it
/// runs yet.
pub fn hello(build: String, pid: u32, start: u64) -> Msg {
    Msg::Hello {
        build,
        window: SECOND_WINDOW.to_string(),
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

/// What runs once the link is made.
pub type OnLinked = Rc<dyn Fn(&LinkDriver)>;
/// The turns running in this window now, as started edges, for its `welcome`.
pub type RunningTurns = Rc<dyn Fn() -> Vec<TurnEdge>>;
/// What runs once the link ended without this window asking.
pub type OnEnded = Rc<dyn Fn(&LinkDriver, &LinkEnd)>;
/// What a `child_exited` from the other window is given to: the shell's pid and start time, and its raw wait status.
pub type OnChildExited = Rc<dyn Fn(u32, u64, Option<i32>)>;
/// What window 1 does when the window 2 it started again at its launch does not come back, with why.
pub type OnRestoreFailed = Rc<dyn Fn(&str)>;

type Hooks = Rc<RefCell<Option<PairHooks>>>;

/// Runs a closure once `Duration` has passed, on the main loop.
pub type After = Rc<dyn Fn(Duration, Box<dyn FnOnce()>)>;

/// How this window starts another window of the pair: the executable it runs, and how long a window 2 it starts
/// has for its handshake, measured by `after`. A window runs its own build ([`Starts::default`]); the tests run
/// stand-ins with timers of their own.
#[derive(Clone)]
pub struct Starts {
    pub exe: PathBuf,
    pub handshake_within: Duration,
    pub after: After,
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
    /// Where the other window's `child_exited` goes.
    child_exited: Rc<RefCell<Option<OnChildExited>>>,
    /// Window 2 only: window 1 started it again with `--restore`.
    restored: std::cell::Cell<bool>,
    /// Window 1 only: a window 2 started again at this launch has not linked yet, so its failure to come back is
    /// still to be handled (once).
    restore_pending: Rc<std::cell::Cell<bool>>,
    restore_failed: Rc<RefCell<Option<OnRestoreFailed>>>,
    /// Window 2 only: the pair lock its `welcome` carried, kept for as long as the window lives, so an orphan can
    /// hand a copy to the window 1 that comes back for it.
    second_lock: Rc<RefCell<Option<PairLock>>>,
    /// Window 2 only, while orphaned: the socket a new window 1 links through.
    orphan: RefCell<Option<Orphan>>,
    /// A window 1 that linked to an orphan before it was built: what the orphan said it holds, for the layout phase.
    relinked: RefCell<Option<BTreeMap<String, String>>>,
    /// That window's link was made before its hooks were set: the `linked` hook is still to run, once they are.
    relink_unannounced: std::cell::Cell<bool>,
    /// How a window this one starts is started.
    starts: RefCell<Starts>,
    /// Window 2 only: the editor window it started that has neither linked nor ended yet, so a second key press
    /// does not start another.
    editor_start: RefCell<Option<relink::StartedEditor>>,
}

/// An orphaned window 2's listener and the connection it is answering, if one is.
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
        let link = link_second(sock, linked_hook(&hooks), lock.clone())?;
        // The link's handler fills the slot once `welcome` comes.
        let pair = Pair::with_hooks(WindowRole::Second { link }, None, root, hooks, lock);
        Ok(pair)
    }

    /// A window 1 that found the pair lock held and linked to the orphaned window 2 holding it ([`relink::relink`]):
    /// window 1 of the pair, with that lock and that link. Its `linked` hook runs once the window sets its hooks.
    pub fn relinked(relinked: Relinked, root: &Path, legacy_backend: bool, clean: bool) -> Rc<Pair> {
        let Relinked {
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

    fn new(role: WindowRole, lock: Option<PairLock>, root: &Path) -> Rc<Pair> {
        Pair::with_hooks(role, lock, root, Rc::default(), Rc::default())
    }

    fn with_hooks(
        role: WindowRole,
        lock: Option<PairLock>,
        root: &Path,
        hooks: Hooks,
        second_lock: Rc<RefCell<Option<PairLock>>>,
    ) -> Rc<Pair> {
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
            child_exited: Rc::default(),
            restored: std::cell::Cell::new(false),
            restore_pending: Rc::default(),
            restore_failed: Rc::default(),
            second_lock,
            orphan: RefCell::new(None),
            relinked: RefCell::new(None),
            relink_unannounced: std::cell::Cell::new(false),
            starts: RefCell::new(Starts::default()),
            editor_start: RefCell::new(None),
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

    /// How a window this one starts is started: stand-ins in the tests.
    #[cfg(test)]
    pub(crate) fn set_starts(&self, starts: Starts) {
        *self.starts.borrow_mut() = starts;
    }

    /// What this window does when the window 2 it starts again at its launch does not come back.
    pub fn on_restore_failed(&self, hook: OnRestoreFailed) {
        *self.restore_failed.borrow_mut() = Some(hook);
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

    /// What the window does on the link's events, from here on. A link made before the window was built (a
    /// re-link) is announced to them now.
    pub fn set_hooks(&self, hooks: PairHooks) {
        *self.hooks.borrow_mut() = Some(hooks);
        if self.relink_unannounced.replace(false) {
            if let Some(link) = self.link().filter(LinkDriver::linked) {
                linked_hook(&self.hooks)(&link);
            }
        }
    }

    /// What the orphan this window linked to at its start said it holds; `None` for a window that did not re-link.
    pub fn relink_holders(&self) -> Option<BTreeMap<String, String>> {
        self.relinked.borrow().clone()
    }

    pub fn owns_editor(&self) -> bool {
        self.role.borrow().owns_editor()
    }

    /// Whether this window holds the project's pair lock as its window 1: the one that reads the pair file at
    /// start and writes it while the pair runs.
    pub fn holds_pair(&self) -> bool {
        self.lock.is_some() && self.role.borrow().writes_main_files()
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

    pub fn is_second(&self) -> bool {
        matches!(*self.role.borrow(), WindowRole::Second { .. })
    }

    pub fn is_unpaired(&self) -> bool {
        matches!(*self.role.borrow(), WindowRole::Unpaired)
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

    fn wire(&self, link: &LinkDriver) {
        let hooks = self.hooks.clone();
        let first = !self.is_second();
        let (pending, failed) = (self.restore_pending.clone(), self.restore_failed.clone());
        link.on_end(move |link, end| {
            // A window 2 started again at this launch that ended before it linked did not come back: the
            // restore's own take-back runs instead.
            if first && link.partner().is_none() && fail_restore(&pending, &failed, "window 2 ended before it linked") {
                return;
            }
            let hook = hooks.borrow().as_ref().map(|h| h.ended.clone());
            if let Some(ended) = hook {
                ended(link, end);
            }
        });
        let peer = eitri_core::wm::PeerId(if self.is_second() { 1 } else { 2 });
        for kind in ["summary", "workspace"] {
            let hooks = self.hooks.clone();
            link.on(kind, move |_, msg, _| {
                let event = crate::gtk_host::peer::pair_event(peer, &msg);
                let post = hooks.borrow().as_ref().map(|h| h.post.clone());
                if let (Some(event), Some(post)) = (event, post) {
                    post(event);
                }
            });
        }
        let hooks = self.hooks.clone();
        link.on("focus", move |_, msg, _| {
            let event = crate::gtk_host::peer::pair_event(peer, &msg);
            let post = hooks.borrow().as_ref().map(|h| h.post.clone());
            if let (Some(event), Some(post)) = (event, post) {
                post(event);
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
        // The pair file names each window's monitor; the other window's comes from here, checked as its input.
        let other_window = if self.is_second() { FIRST_WINDOW } else { SECOND_WINDOW };
        let pair_writer = self.pair_writer.clone();
        link.on("monitor", move |_, msg, _| {
            if let Msg::Monitor { connector, description } = &msg {
                match super::pair_files::peer_monitor(connector, description) {
                    Some(monitor) => {
                        if let Some(writer) = pair_writer.borrow_mut().as_mut() {
                            writer.monitor(other_window, monitor);
                        }
                    }
                    None => eprintln!("[link] the other window names a monitor {connector:?}; not kept"),
                }
            }
            let event = crate::gtk_host::peer::pair_event(peer, &msg);
            let post = hooks.borrow().as_ref().map(|h| h.post.clone());
            if let (Some(event), Some(post)) = (event, post) {
                post(event);
            }
        });
        // What window 2 holds is what the pair file names it the holder of; window 2 hears window 1's held set
        // only to keep its own numbers apart.
        let records_holders = !self.is_second();
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
            let post = hooks.borrow().as_ref().map(|h| h.post.clone());
            if let (Some(event), Some(post)) = (event, post) {
                post(event);
            }
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
                    SecondFlags {
                        legacy: self.legacy_backend.get(),
                        clean: self.clean.get(),
                        restore: false,
                    },
                    lock,
                    linked_hook(&self.hooks),
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
            let linked = linked_hook(&self.hooks);
            Rc::new(move |link: &LinkDriver| {
                pending.set(false);
                linked(link);
            }) as OnLinked
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
            SecondFlags {
                legacy: self.legacy_backend.get(),
                clean: self.clean.get(),
                restore: true,
            },
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
    /// Window 2, orphaned: binds the socket a window 1 starting for the project links through ([`relink`]), in
    /// `dir`, and watches it on the main context. `held` is read for the `welcome`. Nothing is bound in a window
    /// that is not window 2 or holds no pair lock, nor twice.
    pub fn wait_for_relink(self: &Rc<Self>, dir: &Path, held: HeldNow) -> Result<(), String> {
        if !self.is_second() {
            return Err("only window 2 waits for a window 1".into());
        }
        if self.second_lock.borrow().is_none() {
            return Err("window 2 holds no pair lock to hand over".into());
        }
        if self.orphan.borrow().is_some() {
            return Ok(());
        }
        let socket = Rc::new(OrphanSocket::bind(dir, &self.root)?);
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
        println!("[window] window 2 waits for a window 1 at {}", socket.path().display());
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
        let welcome = relink::relink_welcome(&held, running);
        let answered = match self.second_lock.borrow().as_ref() {
            Some(lock) => relink::answer_relink_hello(link, msg, &crate::version::fingerprint(), peer, lock, welcome),
            None => {
                link.close("window 2 holds no pair lock to hand over");
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
        *self.role.borrow_mut() = WindowRole::Second { link: link.clone() };
        // Linked: the editor window started from here is this window 1, or no longer matters; a later start is
        // judged by the link.
        self.editor_start.borrow_mut().take();
        self.wire(link);
        linked_hook(&self.hooks)(link);
    }
}

/// The window controller's `window.new`, carried out by the GTK host: what the pair started or raised is not the
/// controller's to know, since a window 2 it starts becomes its peer only once its `hello` links it.
impl crate::gtk_host::LinkedWindows for Pair {
    fn open(&self, _win: eitri_core::wm::WinId, token: Option<String>) -> Result<(), String> {
        let plan = self.window_new(token)?;
        println!("[window] window.new: {plan:?}");
        Ok(())
    }

    /// Ends the window 2 this window started, as a failed start is ended: SIGTERM, then SIGKILL after the grace, the
    /// application held meanwhile. The link's end then settles whatever was moving. Window 2 did not start window 1,
    /// so it refuses.
    fn end_peer(&self, _win: eitri_core::wm::WinId) -> Result<(), String> {
        if self.is_second() {
            return Err("window 1 was not started by this window; close it there".into());
        }
        let second = self.second.borrow().clone();
        let second = second.ok_or_else(|| NO_OTHER_WINDOW.to_string())?;
        println!(
            "[window] ending window 2 (process {}), which is not answering",
            second.pid()
        );
        if let Some(ending) = end_second(&second, END_GRACE) {
            hold_until_finished(ending);
        }
        Ok(())
    }

    fn restore(&self, _win: eitri_core::wm::WinId) -> Result<(), String> {
        self.restore()?;
        println!("[window] window 2 is started again, as the pair last ended");
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

/// The `linked` hook as the window sets it, read when the link is made.
fn linked_hook(hooks: &Hooks) -> OnLinked {
    let hooks = hooks.clone();
    Rc::new(move |link| {
        let hook = hooks.borrow().as_ref().map(|h| h.linked.clone());
        if let Some(linked) = hook {
            linked(link);
        }
    })
}

/// How long a window 2 told to end has before it is killed: enough to close its files, not enough to keep a start
/// that failed around.
pub const END_GRACE: Duration = Duration::from_secs(2);

/// Window 2's process as the window that started it holds it. The pidfd is opened while the process is still this
/// window's unreaped child, so it names that process and no other: once it has ended, a signal through it reaches
/// nothing, even after its pid is reused. The `Child` itself is waited for on a thread of its own, which reaps it
/// whenever it ends.
pub struct SecondProcess {
    pid: u32,
    start: u64,
    pidfd: Arc<PidFd>,
    #[cfg_attr(not(test), allow(dead_code))] // The window only logs the reap; the tests wait for it.
    reaped: Arc<Reaped>,
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
                            println!("[window] window 2 (process {pid}) ended: {status}");
                            *reaped.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
                            reaped.changed.notify_all();
                        }
                        Err(err) => eprintln!("[window] window 2 (process {pid}) could not be waited for: {err}"),
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

    /// The reaped status, waiting at most `timeout` for it.
    #[cfg(test)]
    fn wait_reaped(&self, timeout: Duration) -> Option<ExitStatus> {
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
    if let Some(done) = terminate(pidfd) {
        return done;
    }
    if pidfd.wait_exited(grace) {
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
    if let Some(done) = terminate(&pidfd) {
        report_ending(pid, &done);
        return;
    }
    let hold = hold_application();
    gtk4::glib::timeout_add_local_once(grace, move || {
        report_ending(pid, &kill_if_running(&pidfd));
        // Released here, after the SIGKILL, rather than whenever GLib frees the finished source.
        drop(hold);
    });
}

/// A hold on the running application, so it stays in its main loop with no window left. Once window 1's last
/// window closes the process would otherwise exit, and with it the thread or timer that still owes a window 2 its
/// SIGKILL: one that ignores SIGTERM would be left running. `None` when no application runs (the unit tests), where
/// nothing exits under the ending.
fn hold_application() -> Option<gtk4::gio::ApplicationHoldGuard> {
    use gtk4::prelude::*;
    gtk4::gio::Application::default().map(|app| app.hold())
}

/// Holds the application until the ending thread has finished, looking from the main loop so nothing waits on the
/// caller; the thread itself is bounded by its grace.
fn hold_until_finished(ending: JoinHandle<Ending>) {
    let Some(hold) = hold_application() else {
        return;
    };
    let mut hold = Some(hold);
    let mut ending = Some(ending);
    gtk4::glib::timeout_add_local(REAP_RETRY, move || {
        if ending.as_ref().is_some_and(|ending| !ending.is_finished()) {
            return gtk4::glib::ControlFlow::Continue;
        }
        if let Some(ending) = ending.take() {
            let _ = ending.join();
        }
        // Released from inside the source, once the ending is done, so the exit it allows never waits on GLib
        // freeing this closure.
        hold.take();
        gtk4::glib::ControlFlow::Break
    });
}

/// Ends `second` with `grace` to go: on a thread when one can be started (the thread, returned), else from the main
/// loop, so a window that ignores SIGTERM is still killed either way.
fn end_second(second: &SecondProcess, grace: Duration) -> Option<JoinHandle<Ending>> {
    match second.end(grace) {
        Ok(ending) => Some(ending),
        Err(err) => {
            // With no thread for the grace, the main loop keeps it.
            eprintln!("[window] window 2 could not be ended on a thread ({err}); the main loop ends it");
            end_from_main_loop(second.pidfd.clone(), second.pid, grace);
            None
        }
    }
}

fn report_ending(pid: u32, ending: &Ending) {
    match ending {
        Ending::Failed(why) => eprintln!("[window] window 2 (process {pid}) could not be ended: {why}"),
        done => println!("[window] window 2 (process {pid}): {done:?}"),
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
        "[window] window 2 (process {}) did not finish its handshake; it is ended",
        second.pid
    );
    end_second(second, grace)
}

/// The flags window 2 is started with. `legacy`: `--legacy`, its chats then running the backend this window's run.
/// `clean`: `--clean`, so it neither reads nor writes the remembered state this launch leaves alone. `restore`:
/// `--restore`, started again as the pair last ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecondFlags {
    pub legacy: bool,
    pub clean: bool,
    pub restore: bool,
}

/// Window 1: makes the socketpair, starts window 2 from `starts`' executable (this one's, in a window) with the
/// other end and `flags`, and links when its `hello` matches. `linked` runs once `welcome` is sent. The child is
/// reaped on a thread of its own, and ended if it has not finished the handshake in `starts`' time
/// ([`HANDSHAKE_WITHIN`] in a window); `overdue`, when given, runs then if the link is not linked.
#[allow(clippy::too_many_arguments)]
pub fn start_second(
    starts: &Starts,
    root: &Path,
    token: Option<&str>,
    flags: SecondFlags,
    lock: Rc<PairLock>,
    linked: OnLinked,
    running: Rc<RefCell<Option<RunningTurns>>>,
    overdue: Option<Rc<dyn Fn()>>,
) -> io::Result<(LinkDriver, Rc<SecondProcess>)> {
    let (mine, theirs) = fdpass::seqpacket_pair()?;
    let second = Rc::new(SecondProcess::started(spawn_second(
        &starts.exe,
        theirs,
        root,
        token,
        flags.legacy,
        flags.clean,
        flags.restore,
    )?)?);
    let (pid, start) = (second.pid(), second.start());
    let presence = not_yet_confirmed();
    let (handle, inbound) = match LinkHandle::start(mine, presence.clone()) {
        Ok(started) => started,
        Err(err) => {
            // No link means no handshake and no deadline to end it by: a window 2 hung before its toolkit came up
            // would never see its socket close, so it is ended here.
            eprintln!("[window] the link to window 2 (process {pid}) could not be started ({err}); it is ended");
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
        pid,
        start,
    };
    link.on("hello", move |link, msg, _| {
        let running = running.borrow().clone();
        let running = move || running.as_ref().map(|running| running()).unwrap_or_default();
        answer_hello(link, &msg, &expected, &lock, &linked, &running)
    });
    link.watch();
    // A window 2 that never says hello (it hung before its toolkit came up, or died in a way the link does not
    // see) would otherwise leave this link up for good, and `window.new` could never start another; its process,
    // left running, would keep whatever it opened.
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
    println!("[window] started window 2 as process {pid}");
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
    linked: &OnLinked,
    running: &dyn Fn() -> Vec<TurnEdge>,
) {
    if link.linked() {
        eprintln!("[link] a second hello on a linked link; ignored");
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
    println!("[link] linked to window 2 (process {})", partner.pid);
    linked(link);
}

/// Window 2: starts the link on its checked end, says `hello`, and links on `welcome`, adopting the pair lock it
/// carries into `lock`, which the window keeps, so the lock is held for as long as this window lives, after the link
/// ended too. `linked` runs once linked.
pub fn link_second(sock: OwnedFd, linked: OnLinked, lock: Rc<RefCell<Option<PairLock>>>) -> io::Result<LinkDriver> {
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
            take_welcome(link, &msg, fds, parent, &lock, &linked);
        });
    }
    link.send(hello(crate::version::fingerprint(), pid, start), Vec::new())
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
    linked: &OnLinked,
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
        Ok(adopted) => *lock.borrow_mut() = Some(adopted),
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
mod tests {
    use super::*;
    use crate::linked_window::driver::tests::{pair, pump_until};
    use eitri_core::window_link::link::Inbound;
    use std::time::Duration;

    fn expected() -> Expected {
        Expected {
            build: "0.4.0+abc".into(),
            pid: 4242,
            start: 99,
        }
    }

    #[test]
    fn the_handshake_refuses_another_build() {
        let ok = hello("0.4.0+abc".into(), 4242, 99);
        assert_eq!(check_hello(&ok, &expected()), Ok(Partner { pid: 4242, start: 99 }));

        let other_build = hello("0.4.1+def".into(), 4242, 99);
        let err = check_hello(&other_build, &expected()).unwrap_err();
        assert!(err.contains("another build"), "{err}");

        let other_child = hello("0.4.0+abc".into(), 4243, 99);
        assert!(check_hello(&other_child, &expected())
            .unwrap_err()
            .contains("not the window started here"));
        let reused_pid = hello("0.4.0+abc".into(), 4242, 100);
        assert!(
            check_hello(&reused_pid, &expected()).is_err(),
            "the same pid started at another time is another process"
        );
        let mut window_3 = hello("0.4.0+abc".into(), 4242, 99);
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
        let linked: OnLinked = {
            let calls = linked_calls.clone();
            Rc::new(move |_| calls.set(calls.get() + 1))
        };
        let exp = expected();
        driver.on("hello", move |link, msg, _| {
            answer_hello(link, &msg, &exp, &lock, &linked, &Vec::new)
        });
        theirs.send(hello("0.9.9+zzz".into(), 4242, 99), Vec::new()).unwrap();
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
        let linked: OnLinked = {
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
        theirs.send(hello("0.4.0+abc".into(), 4242, 99), Vec::new()).unwrap();
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
        assert!(
            linked.wait_reaped(Duration::from_millis(300)).is_none(),
            "the linked window still runs"
        );
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

    /// An application whose last window has closed must not exit while a window 2 still waits for its SIGKILL: `run`
    /// returns only once the ending is done, on the thread and on the main-loop timer alike.
    ///
    /// Ignored because `g_application_run` iterates the default main context, which two tests in two threads would
    /// fight over. Run it alone:
    /// `cargo test -p shell the_application_outlives_an_ending_in_its_grace -- --ignored --exact`.
    #[test]
    #[ignore = "runs a real gio main loop on the default main context; must not race the rest of the suite"]
    fn the_application_outlives_an_ending_in_its_grace() {
        use gtk4::gio;
        use gtk4::prelude::*;
        let grace = Duration::from_millis(400);
        let app = gio::Application::new(None, gio::ApplicationFlags::empty());
        let stubborn = Rc::new(stand_in(true));
        let on_timer = Rc::new(stand_in(true));
        {
            let (stubborn, on_timer) = (stubborn.clone(), on_timer.clone());
            // Nothing else holds the application: without the holds, `run` would return as soon as this does.
            app.connect_activate(move |_| {
                hold_until_finished(stubborn.end(grace).unwrap());
                end_from_main_loop(on_timer.pidfd.clone(), on_timer.pid, grace);
            });
        }
        let started = Instant::now();
        app.run_with_args(&["shell-ending-hold"]);
        let ran = started.elapsed();
        assert!(ran >= grace, "run() returned after {ran:?}, inside the grace");
        for second in [&stubborn, &on_timer] {
            let status = second.wait_reaped(Duration::from_secs(10)).expect("reaped");
            assert_eq!(signal_of(status), Some(libc::SIGKILL), "{status:?}");
        }
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
                linked: Rc::new(|_| {}),
                post: Rc::new(move |event| saved.borrow_mut().push(event)),
                ended: Rc::new(|_, _| {}),
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
            reported.recv_timeout(Duration::from_secs(5)).is_err(),
            "nothing is reported"
        );
    }

    #[test]
    fn a_focus_for_a_module_from_the_other_window_reaches_the_controller() {
        use eitri_core::wm::fake::Logged;
        use eitri_core::wm::{ActivationToken, HostOp, WinId};
        let (wm, host) = crate::linked_window::driver::tests::controller();
        let (driver, theirs, _inbound) = pair();
        driver.attach_controller(WinId::MAIN, eitri_core::wm::PeerId(2), &wm);
        let pair = Pair::new(
            WindowRole::Main {
                link: Some(driver.clone()),
            },
            None,
            Path::new("/p"),
        );
        let posted = Rc::downgrade(&wm);
        pair.set_hooks(PairHooks {
            linked: Rc::new(|_| {}),
            post: Rc::new(move |event| {
                if let Some(wm) = posted.upgrade() {
                    wm.post(eitri_core::wm::HostEvent::Peer(event));
                }
            }),
            ended: Rc::new(|_, _| {}),
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
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::linked_window::tests::Scratch::new(label);
        let report = dir.1.join("pid");
        let exe = dir.1.join("window-2-stand-in");
        std::fs::write(&exe, format!("#!/bin/sh\necho $$ > '{}'\n{script}\n", report.display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
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
                linked: Rc::new(|_| {}),
                post: Rc::new(|_| {}),
                ended: Rc::new(move |_, end| ended.borrow_mut().push(end.clone())),
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
}
