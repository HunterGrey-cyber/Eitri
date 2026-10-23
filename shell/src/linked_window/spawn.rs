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
//! child, so nothing but that process can ever be signalled.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
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

    /// Raises the other window and gives it the keys. The window controller's `window.other` sends the same
    /// `focus{window}` itself, once the host has asked the activation token; this stays for the tests of the pair.
    #[cfg_attr(not(test), allow(dead_code))]
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

/// The window controller's `window.new`, carried out by the GTK host: what the pair started or raised is not the
/// controller's to know, since a window 2 it starts becomes its peer only once its `hello` links it.
impl crate::gtk_host::LinkedWindows for Pair {
    fn open(&self, _win: eitri_core::wm::WinId, token: Option<String>) -> Result<(), String> {
        let plan = self.window_new(token)?;
        println!("[window] window.new: {plan:?}");
        Ok(())
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

/// Window 1: makes the socketpair, starts window 2 from this executable with the other end, and links when its
/// `hello` matches. `linked` runs once `welcome` is sent. The child is reaped on a thread of its own, and ended if it
/// has not finished the handshake in [`HANDSHAKE_WITHIN`]. `legacy`: window 2 is started with `--legacy`, its chats
/// then running the backend this window's run.
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
    let second = SecondProcess::started(spawn_second(Path::new("/proc/self/exe"), theirs, root, token, legacy)?)?;
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
    link.set_handshake_deadline(Instant::now() + HANDSHAKE_WITHIN);
    {
        let link = link.clone();
        gtk4::glib::timeout_add_local_once(HANDSHAKE_WITHIN, move || {
            // The ending thread is bounded by the grace; the application is held until it has finished.
            if let Some(ending) = end_if_overdue(&link, &second, Instant::now(), END_GRACE) {
                hold_until_finished(ending);
            }
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
}
