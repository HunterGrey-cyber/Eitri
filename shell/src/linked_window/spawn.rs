//! How the two windows link: window 1 starts window 2 and answers its `hello`, window 2 says `hello` and waits for
//! `welcome`.
//!
//! Window 1 makes the socketpair, starts its own executable as window 2 with the other end, and expects a `hello`
//! from exactly that child, of exactly this build: the child's pid and start time name it even if another process
//! somehow held the other end, and the build fingerprint keeps two builds from sharing one pair's state. A `hello`
//! that does not match is answered with `bye` and the link closes. A matching one is answered with `welcome`,
//! which carries a copy of the pair lock's descriptor, so the lock stays held while either window lives.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::window_link::fdpass;
use eitri_core::window_link::link::{LinkHandle, PresenceFlag};
use eitri_core::window_link::pair::PairLock;
use eitri_core::window_link::start::{spawn_second, SECOND_WINDOW};
use eitri_core::window_link::{FocusTarget, Msg, Summary, TurnEdge};

use super::driver::{LinkDriver, Partner, HANDSHAKE_WITHIN};
use super::pair_files::PairWriter;
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
    /// `focus{window}`: raise this window, with the other window's activation token when it sent one.
    pub raise: Rc<dyn Fn(Option<String>)>,
    pub ended: OnEnded,
}

/// What runs once the link is made.
pub type OnLinked = Rc<dyn Fn(&LinkDriver)>;
/// The turns running in this window now, as started edges, for its `welcome`.
pub type RunningTurns = Rc<dyn Fn() -> Vec<TurnEdge>>;
/// What runs once the link ended without this window asking.
pub type OnEnded = Rc<dyn Fn(&LinkDriver, &LinkEnd)>;

type Hooks = Rc<RefCell<Option<PairHooks>>>;

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
    /// The turns running here, for the `welcome` of a window 2 this window starts.
    running: Rc<RefCell<Option<RunningTurns>>>,
}

impl Pair {
    /// A full window: window 1 of the pair with the lock `taken`, else unpaired or alone
    /// ([`WindowRole::of_full_window`]). `legacy_backend`: this launch's chats run the legacy backend.
    pub fn full(taken: io::Result<Option<PairLock>>, root: &Path, legacy_backend: bool) -> Rc<Pair> {
        let (role, lock) = WindowRole::of_full_window(taken);
        let pair = Pair::new(role, lock, root);
        pair.legacy_backend.set(legacy_backend);
        pair
    }

    /// Window 2, on its checked link end: says `hello` at once and links on `welcome`.
    pub fn second(sock: OwnedFd, root: &Path) -> io::Result<Rc<Pair>> {
        let hooks: Hooks = Rc::default();
        let link = link_second(sock, linked_hook(&hooks))?;
        let pair = Pair::with_hooks(WindowRole::Second { link }, None, root, hooks);
        Ok(pair)
    }

    fn new(role: WindowRole, lock: Option<PairLock>, root: &Path) -> Rc<Pair> {
        Pair::with_hooks(role, lock, root, Rc::default())
    }

    fn with_hooks(role: WindowRole, lock: Option<PairLock>, root: &Path, hooks: Hooks) -> Rc<Pair> {
        let pair = Rc::new(Pair {
            role: RefCell::new(role),
            lock: lock.map(Rc::new),
            root: root.to_path_buf(),
            hooks,
            activation: RefCell::new(None),
            pair_writer: Rc::default(),
            legacy_backend: std::cell::Cell::new(false),
            running: Rc::default(),
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

    /// Where the turns running in this window are read when it welcomes a window 2.
    pub fn set_running_turns(&self, running: RunningTurns) {
        *self.running.borrow_mut() = Some(running);
    }

    /// What the window does on the link's events, from here on.
    pub fn set_hooks(&self, hooks: PairHooks) {
        *self.hooks.borrow_mut() = Some(hooks);
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
        link.on_end(move |link, end| {
            let hook = hooks.borrow().as_ref().map(|h| h.ended.clone());
            if let Some(ended) = hook {
                ended(link, end);
            }
        });
        let hooks = self.hooks.clone();
        link.on("focus", move |_, msg, _| {
            let Msg::Focus { target, token } = msg else { return };
            match target {
                FocusTarget::Window => {
                    let hook = hooks.borrow().as_ref().map(|h| h.raise.clone());
                    if let Some(raise) = hook {
                        raise(token);
                    }
                }
                other => println!("[link] the other window asked for {other:?}; this window does not take it yet"),
            }
        });
        // What window 2 holds is what the pair file names it the holder of; window 2 hears window 1's held set
        // only to keep its own numbers apart.
        let records_holders = !self.is_second();
        let pair_writer = self.pair_writer.clone();
        link.on("away", move |_, msg, _| {
            if let Msg::Away { ids } = msg {
                println!("[link] the other window holds {}", ids.join(", "));
                if records_holders {
                    if let Some(writer) = pair_writer.borrow_mut().as_mut() {
                        writer.window2_holds(&ids.iter().cloned().collect());
                    }
                }
            }
        });
    }

    /// `window.new`: in window 1, starts window 2 (or raises it when it is up); `token` is the activation token
    /// the key press gave, so the window that opens gets the keys. Refused in an unpaired window and in one that
    /// cannot pair.
    #[cfg_attr(not(test), allow(dead_code))] // Called by the window controller's window verbs.
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
                let link = start_second(
                    &self.root,
                    token.as_deref(),
                    self.legacy_backend.get(),
                    lock,
                    linked_hook(&self.hooks),
                    self.running.clone(),
                )
                .map_err(|e| format!("the second window could not be started: {e}"))?;
                self.wire(&link);
                *self.role.borrow_mut() = WindowRole::Main { link: Some(link) };
            }
        }
        Ok(plan)
    }

    /// `window.other`: raises the other window and gives it the keys.
    #[cfg_attr(not(test), allow(dead_code))] // Called by the window controller's window verbs.
    pub fn window_other(&self, token: Option<String>) -> Result<(), String> {
        self.focus_other(token)
    }

    fn focus_other(&self, token: Option<String>) -> Result<(), String> {
        let link = self
            .link()
            .filter(LinkDriver::linked)
            .ok_or_else(|| NO_OTHER_WINDOW.to_string())?;
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

/// Window 1: makes the socketpair, starts window 2 from this executable with the other end, and links when its
/// `hello` matches. `linked` runs once `welcome` is sent. The child is reaped on a thread of its own. `legacy`:
/// window 2 is started with `--legacy`, its chats then running the backend this window's run.
pub fn start_second(
    root: &Path,
    token: Option<&str>,
    legacy: bool,
    lock: Rc<PairLock>,
    linked: OnLinked,
    running: Rc<RefCell<Option<RunningTurns>>>,
) -> io::Result<LinkDriver> {
    let (mine, theirs) = fdpass::seqpacket_pair()?;
    // The running executable, not the installed path: an upgrade that replaced the file still starts this build.
    let mut child = spawn_second(Path::new("/proc/self/exe"), theirs, root, token, legacy)?;
    let pid = child.id();
    let start = match agent::child::start_time(pid) {
        Ok(start) => start,
        Err(err) => {
            // Without its start time the child cannot be told from a process that later takes its pid.
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    std::thread::Builder::new()
        .name("window-2-reaper".into())
        .spawn(move || match child.wait() {
            Ok(status) => println!("[window] window 2 (process {pid}) ended: {status}"),
            Err(err) => eprintln!("[window] window 2 (process {pid}) could not be waited for: {err}"),
        })?;
    let presence = not_yet_confirmed();
    let (handle, inbound) = LinkHandle::start(mine, presence.clone())?;
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
    // see) would otherwise leave this link up for good, and `window.new` could never start another.
    link.set_handshake_deadline(std::time::Instant::now() + HANDSHAKE_WITHIN);
    {
        let link = link.clone();
        gtk4::glib::timeout_add_local_once(HANDSHAKE_WITHIN, move || {
            link.handshake_overdue(std::time::Instant::now());
        });
    }
    println!("[window] started window 2 as process {pid}");
    Ok(link)
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
/// carries. The lock is kept by the link's handler, so it is held for as long as this window keeps its link, after
/// the link ended too. `linked` runs once linked.
pub fn link_second(sock: OwnedFd, linked: OnLinked) -> io::Result<LinkDriver> {
    // SAFETY: neither call has preconditions or can fail.
    let (pid, parent) = unsafe { (libc::getpid(), libc::getppid()) };
    let (pid, parent) = (pid as u32, parent as u32);
    let start = agent::child::start_time(pid)?;
    let presence = not_yet_confirmed();
    let (handle, inbound) = LinkHandle::start(sock, presence.clone())?;
    let link = LinkDriver::new(handle, inbound);
    link.set_presence(presence);
    let lock: Rc<RefCell<Option<PairLock>>> = Rc::default();
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
}
