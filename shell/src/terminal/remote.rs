//! Window-owned server terminal state. This model holds no toolkit objects; disk and link work runs on workers.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::remote_client::HelperConnections;
use eitri_core::onboarding::display::display_text;
use eitri_core::remote_project::terminal_facts::TerminalFactsHandle;
use eitri_core::remote_project::terminal_link::{OpenAnswer, TerminalLinkOptions};
use eitri_core::remote_project::terminals::TerminalEntry;
use eitri_core::remote_project::{RemoteConnection, RemoteProject, RemoteProjects, WindowOrigin};
use eitri_core::wm::ShellIdentity;
use eitri_terminal::headless::WaitWhy;
use eitri_terminal::mirror::{HistoryNote, LinkState, MirrorConfig, MirrorEnd, MirrorReport, TermLink};
use eitri_terminal::{ExitInfo, PtySize, TerminalColors, TerminalSession};
use terminal_render::PaintOp;

pub(crate) enum TermSource {
    Local,
    Server(RemoteConnection),
    Waiting,
}

/// Each process has its own window and registry, including the second window of a pair.
pub(crate) struct Registry<C = RemoteConnection> {
    origin: RefCell<WindowOrigin>,
    connection: RefCell<Option<C>>,
    /// The window's place in the project's terminal record: 1 for the window owning the first slot, 2 for window 2.
    /// `None` for a window that binds no server terminal at all (a chat or page window, or a second full window of
    /// the project): borrowing another window's slot would let it restore, offer or stop that window's shells.
    /// Taken again (`retry_slot`) when a pane of a window that may own the first slot is about to start, so a window
    /// opened while the previous owner was still exiting gets it once that one has gone.
    slot: Cell<Option<u8>>,
    window: Cell<TerminalWindow>,
    /// Why this window has no slot, said when one of its panes would start a server terminal.
    no_slot: Cell<&'static str>,
    /// Held while this window owns the first slot of its server project.
    first_slot: RefCell<Option<agent::private_fs::PrivateLock>>,
    /// The host key and root the window's start tried the lock for: every retry tries that lock, so a host key the
    /// window accepted later cannot lead it to a lock its old owner does not hold.
    slot_key: RefCell<Option<(String, PathBuf)>>,
    /// Slot 1 taken without the lock (a window with no pair lock whose first try could not open the lock file); the
    /// lock is tried again on every retry while this holds.
    blind: Cell<bool>,
    pub(crate) facts: TerminalFactsHandle,
    store: Arc<RemoteProjects>,
    listeners: RefCell<BTreeMap<u64, Rc<dyn Fn()>>>,
    next_listener: Cell<u64>,
    band: Rc<dyn Fn(String)>,
    /// Where a warning goes that must be seen whatever the window shows (the window's toast); the band until set.
    warning: RefCell<Option<Rc<dyn Fn(String)>>>,
    adoption: RefCell<Option<String>>,
    offer: RefCell<Option<Rc<dyn Fn()>>>,
}

/// Why a pane whose module number has an unclaimed saved terminal does not start: that shell is offered, never
/// attached because a number was reused.
pub(crate) const UNCLAIMED_REFUSAL: &str =
    "this module has an unclaimed server terminal; open it from the terminal offer";

impl<C: Clone> Registry<C> {
    fn new(origin: WindowOrigin, slot: Option<u8>, band: Rc<dyn Fn(String)>) -> Self {
        Self {
            origin: RefCell::new(origin),
            connection: RefCell::new(None),
            window: Cell::new(match slot {
                Some(2) => TerminalWindow::Second,
                Some(_) => TerminalWindow::Full,
                None => TerminalWindow::Chat,
            }),
            slot: Cell::new(slot),
            no_slot: Cell::new(NO_SERVER_TERMINALS),
            first_slot: RefCell::new(None),
            slot_key: RefCell::new(None),
            blind: Cell::new(false),
            facts: TerminalFactsHandle::default(),
            store: Arc::new(RemoteProjects::new(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            )),
            listeners: RefCell::new(BTreeMap::new()),
            next_listener: Cell::new(0),
            band,
            warning: RefCell::new(None),
            adoption: Default::default(),
            offer: RefCell::new(None),
        }
    }

    /// Decides the window's slot. A full window of a server project, and one with no pair lock at all, takes the
    /// project's first-slot lock: a small local file lock taken without waiting, as the pair lock is taken at the
    /// window's start.
    fn claim_slot(&self, window: TerminalWindow) {
        self.window.set(window);
        *self.slot_key.borrow_mut() = self
            .origin
            .borrow()
            .remote()
            .map(|project| (project.host_key_sha256.clone(), project.root.clone()));
        self.slot.set(match window {
            TerminalWindow::Second => Some(2),
            TerminalWindow::Chat => None,
            TerminalWindow::Alone | TerminalWindow::Full => match self.take_first_slot() {
                Ok(slot) => slot,
                // Only at the start, and only a window with no pair lock to take: it could not tell another window
                // apart before the slot lock existed either, and the records are likely unusable as well. It tries
                // the lock again later.
                Err(()) if window == TerminalWindow::Alone => {
                    self.blind.set(true);
                    Some(1)
                }
                Err(()) => {
                    self.no_slot.set(SLOT_LOCK_FAILED);
                    None
                }
            },
        });
    }

    /// One try at the first slot's lock, for the key and root of the window's start. The lock is exclusive, so of two
    /// windows trying at once only one gets it. `Err` when the lock could not be tried at all.
    fn take_first_slot(&self) -> Result<Option<u8>, ()> {
        let Some((key, root)) = self.slot_key.borrow().clone() else {
            return Ok(Some(1));
        };
        match self.store.terminals().take_first_slot(&key, &root) {
            Ok(Some(lock)) => {
                *self.first_slot.borrow_mut() = Some(lock);
                self.blind.set(false);
                Ok(Some(1))
            }
            Ok(None) => {
                self.no_slot.set(ANOTHER_WINDOW_OWNS);
                Ok(None)
            }
            Err(error) => {
                eprintln!("[terminal] the server terminals' ownership lock: {error}");
                Err(())
            }
        }
    }

    /// Tries the first slot again for a window that may own it and does not; `true` when it now does. The caller
    /// wakes the window's panes then ([`Registry::wake`]), so each restores what it skipped and starts. An error
    /// never gives a slot here. A window holding slot 1 blind tries the lock again and keeps slot 1 whatever comes of
    /// it (`false`: nothing changed for its panes).
    pub(crate) fn retry_slot(&self) -> bool {
        if self.blind.get() {
            let _ = self.take_first_slot();
            return false;
        }
        if self.slot.get().is_some() || !matches!(self.window.get(), TerminalWindow::Alone | TerminalWindow::Full) {
            return false;
        }
        let slot = self.take_first_slot().unwrap_or_else(|()| {
            self.no_slot.set(SLOT_LOCK_FAILED);
            None
        });
        self.slot.set(slot);
        slot.is_some()
    }

    /// Whether a retry may change anything: no slot yet in a window that may own the first, or slot 1 held blind.
    pub(crate) fn may_retry_slot(&self) -> bool {
        self.blind.get()
            || (self.slot.get().is_none() && matches!(self.window.get(), TerminalWindow::Alone | TerminalWindow::Full))
    }

    pub(crate) fn slot(&self) -> Option<u8> {
        self.slot.get()
    }

    /// Runs every pane's wake (restore, then start if wanted) and the offer.
    pub(crate) fn wake(&self) {
        let listeners: Vec<_> = self.listeners.borrow().values().cloned().collect();
        for listener in listeners {
            listener();
        }
    }

    fn hand_off(&self, project: RemoteProject, connection: C) -> bool {
        let accepted = self
            .origin
            .borrow()
            .remote()
            .is_some_and(|old| old.host == project.host && old.root == project.root);
        if !accepted {
            return false;
        }
        *self.origin.borrow_mut() = WindowOrigin::Remote(project);
        *self.connection.borrow_mut() = Some(connection);
        self.wake();
        true
    }
}

thread_local! {
    static WINDOW: RefCell<Option<Rc<Registry>>> = const { RefCell::new(None) };
}

/// What a window is to the project's terminal record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalWindow {
    /// Window 2: the second slot (only the pair's window 1 starts one).
    Second,
    /// A chat or page window (one window 1 started, or one beside a person's own nvim): no slot.
    Chat,
    /// A full window that found no pair lock to take at all: the first slot, since it cannot tell whether another
    /// window runs.
    Alone,
    /// Window 1 or a full window of a project whose pair lock another window holds: the first slot only while it
    /// holds the project's first-slot lock.
    Full,
}

pub(crate) fn terminal_window(window_two: bool, module_window: bool, alone: bool) -> TerminalWindow {
    match (module_window, window_two, alone) {
        (true, _, _) => TerminalWindow::Chat,
        (false, true, _) => TerminalWindow::Second,
        (false, false, true) => TerminalWindow::Alone,
        (false, false, false) => TerminalWindow::Full,
    }
}

/// Server terminals open only in a slot-owning window; in any other the pane says so and starts nothing.
pub(crate) const NO_SERVER_TERMINALS: &str =
    "server terminals open in the project's main window or window 2, not in a chat or page window";

/// Why a second full window of a server project starts no server terminal.
pub(crate) const ANOTHER_WINDOW_OWNS: &str =
    "this project's server terminals belong to its other open window; use them there";

/// Why a full window whose first-slot lock could not be taken starts no server terminal.
pub(crate) const SLOT_LOCK_FAILED: &str =
    "server terminals are off in this window: their ownership lock could not be taken (see the log)";

pub(crate) fn prepare(origin: &WindowOrigin, window: TerminalWindow, band: Rc<dyn Fn(String)>) -> Rc<Registry> {
    let registry = Registry::new(origin.clone(), None, band);
    registry.claim_slot(window);
    let registry = Rc::new(registry);
    WINDOW.with(|window| *window.borrow_mut() = Some(registry.clone()));
    registry
}

pub(crate) fn connected(connection: RemoteConnection) {
    let registry = WINDOW.with(|window| window.borrow().clone());
    if let Some(registry) = registry {
        registry.hand_off(connection.project().clone(), connection);
    }
}

/// A pane not yet bound to its window's registry. In a window of a server project it waits as a bound pane waits for
/// the connection: only a binding starts a server terminal, and a server project's window never starts a local shell.
pub(crate) fn unbound_source() -> TermSource {
    let remote = WINDOW.with(|window| {
        window
            .borrow()
            .as_ref()
            .is_some_and(|registry| registry.origin.borrow().remote().is_some())
    });
    if remote {
        TermSource::Waiting
    } else {
        TermSource::Local
    }
}

pub(crate) fn source_for(origin: &WindowOrigin) -> TermSource {
    let Some(project) = origin.remote() else {
        return TermSource::Local;
    };
    let connection = WINDOW.with(|window| {
        window.borrow().as_ref().and_then(|registry| {
            let matches = registry
                .origin
                .borrow()
                .remote()
                .is_some_and(|current| current.host == project.host && current.root == project.root);
            matches.then(|| registry.connection.borrow().clone()).flatten()
        })
    });
    connection.map(TermSource::Server).unwrap_or(TermSource::Waiting)
}

impl Registry {
    pub(crate) fn origin(&self) -> WindowOrigin {
        self.origin.borrow().clone()
    }

    pub(crate) fn listen(&self, wake: Rc<dyn Fn()>) -> u64 {
        let token = self.next_listener.get();
        self.next_listener
            .set(token.checked_add(1).expect("terminal listener tokens exhausted"));
        self.listeners.borrow_mut().insert(token, wake);
        token
    }

    /// Whether panes of this window may own server terminals: false in a chat or page window and in a second full
    /// window of the project.
    pub(crate) fn binds_terminals(&self) -> bool {
        self.slot.get().is_some()
    }

    /// Whether this window can own a slot at all, now or once another window lets the first one go: false only in
    /// a chat or page window.
    pub(crate) fn may_bind_terminals(&self) -> bool {
        self.window.get() != TerminalWindow::Chat
    }

    pub(crate) fn no_slot_reason(&self) -> &'static str {
        self.no_slot.get()
    }

    pub(crate) fn adopting(&self) -> bool {
        self.adoption.borrow().is_some()
    }

    pub(crate) fn notice(&self, text: String) {
        (self.band)(text);
    }

    /// The window's own place for a warning: the agent panel's band is gone with a hidden or unloaded panel.
    pub(crate) fn set_warning(&self, show: Rc<dyn Fn(String)>) {
        *self.warning.borrow_mut() = Some(show);
    }

    /// A warning the person must see (programs of a closed server terminal may still run): on the window's toast,
    /// and in the log, since a closing window may take the toast with it.
    pub(crate) fn warn(&self, text: String) {
        eprintln!("[terminal] {text}");
        let show = self.warning.borrow().clone();
        match show {
            Some(show) => show(text),
            None => (self.band)(text),
        }
    }

    /// What a close of a server terminal answered: a refusal on the band, the cleanup warning where it is always seen.
    pub(crate) fn report_close(&self, answer: Result<Option<String>, String>) {
        match answer {
            Err(why) => self.notice(display_text(&why)),
            Ok(Some(warning)) => self.warn(warning),
            Ok(None) => {}
        }
    }

    /// What "Stop them" answered: every cleanup warning, then every failure, whichever stops succeeded or failed.
    pub(crate) fn report_stops(&self, (warnings, failures): (Vec<String>, Vec<String>)) {
        for warning in warnings {
            self.warn(warning);
        }
        if !failures.is_empty() {
            self.notice(format!("terminal stops: {}", display_text(&failures.join("; "))));
        }
    }

    /// The window's unclaimed-terminal offer, run again when a pane is refused for one (the offer may have been put
    /// off with "Later" or never shown since the connection).
    pub(crate) fn set_offer(&self, offer: Rc<dyn Fn()>) {
        *self.offer.borrow_mut() = Some(offer);
    }

    pub(crate) fn offer_again(&self) {
        let offer = self.offer.borrow().clone();
        if let Some(offer) = offer {
            offer();
        }
    }

    /// Only a binding created by this controller call may take this adoption. A refused creation leaves none pending.
    pub(crate) fn adopt(&self, entry: &TerminalEntry, create: impl FnOnce()) {
        *self.adoption.borrow_mut() = Some(entry.command_id.clone());
        create();
        self.adoption.borrow_mut().take();
    }

    pub(crate) fn owner(&self, module: String) -> Option<Owner> {
        Some(Owner {
            project: self.origin.borrow().remote()?.clone(),
            slot: self.slot.get()?,
            module,
            store: self.store.clone(),
            facts: self.facts.clone(),
            claimed: Default::default(),
            adoption: Default::default(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct Owner {
    pub(crate) project: RemoteProject,
    slot: u8,
    module: String,
    store: Arc<RemoteProjects>,
    pub(crate) facts: TerminalFactsHandle,
    claimed: Arc<Mutex<Option<TerminalEntry>>>,
    adoption: Arc<Mutex<Option<String>>>,
}

/// Serializes an opening, its durable answer and a person's close on one worker.
#[derive(Clone)]
pub(crate) struct Work(std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>);

impl Default for Work {
    fn default() -> Self {
        let (send, receive) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("terminal-record".into())
            .spawn(move || {
                while let Ok(work) = receive.recv() {
                    work();
                }
            })
            .expect("terminal record worker");
        Self(send)
    }
}

impl Work {
    pub(crate) fn run(&self, work: impl FnOnce() + Send + 'static) {
        let _ = self.0.send(Box::new(work));
    }
}

pub(crate) struct Binding {
    pub(crate) registry: Rc<Registry>,
    listener: Option<u64>,
    pub(crate) restore: bool,
    pub(crate) retry_entry: Option<TerminalEntry>,
    pub(crate) model: Model,
    pub(crate) work: Work,
    pub(crate) cancelled: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) starting: bool,
    pub(crate) ownership_known: bool,
    pub(crate) entry: Option<TerminalEntry>,
    pub(crate) owner: Option<Owner>,
    pub(crate) again: Option<Again>,
    /// The running session's link handle: whether its ending means the terminal no longer exists.
    pub(crate) answer: Option<OpenAnswer>,
    module: String,
    /// Bound at the window's start: its saved entry is read once the binding has an owner.
    pub(crate) wants_restore: bool,
}

impl Binding {
    pub(crate) fn new(registry: Rc<Registry>, module: String) -> Self {
        let host = registry.origin().remote().map(|p| p.host.clone()).unwrap_or_default();
        let mut owner = registry.owner(module.clone());
        if let Some(owner) = owner.as_mut() {
            *owner.adoption.lock().unwrap() = registry.adoption.borrow_mut().take();
        }
        Self {
            registry,
            listener: None,
            restore: false,
            retry_entry: None,
            model: Model::new(&host),
            work: Work::default(),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            starting: false,
            ownership_known: false,
            entry: None,
            owner,
            again: None,
            answer: None,
            module,
            wants_restore: false,
        }
    }
}

impl Binding {
    pub(crate) fn listen(&mut self, wake: Rc<dyn Fn()>) {
        if let Some(token) = self.listener.take() {
            self.registry.listeners.borrow_mut().remove(&token);
        }
        self.listener = Some(self.registry.listen(wake));
    }

    /// Reused idle surfaces keep their listener, but this instance receives a fresh ownership handle.
    pub(crate) fn adopt_pending(&mut self) {
        let Some(command) = self.registry.adoption.borrow_mut().take() else {
            return;
        };
        let Some(owner) = self.registry.owner(self.module.clone()) else {
            return;
        };
        *owner.adoption.lock().unwrap() = Some(command);
        self.owner = Some(owner);
        self.entry = None;
        self.retry_entry = None;
        self.again = None;
        self.restore = false;
        self.ownership_known = true;
        self.cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    }

    /// Gives a binding made while its window had no slot an owner once the window has one, and then reads its saved
    /// entry as a binding made with one would have. `false` while the window still has none.
    pub(crate) fn ensure_owner(&mut self) -> bool {
        if self.owner.is_some() {
            return true;
        }
        let Some(owner) = self.registry.owner(self.module.clone()) else {
            return false;
        };
        self.owner = Some(owner);
        if self.wants_restore && self.entry.is_none() && !self.starting {
            self.restore = true;
            self.ownership_known = false;
        }
        true
    }

    pub(crate) fn current_owner(&self) -> Option<Owner> {
        let mut owner = self.owner.clone()?;
        owner.project = self.registry.origin().remote()?.clone();
        Some(owner)
    }

    pub(crate) fn claims_module(&self) -> bool {
        self.starting
            || (self.model.opening && self.model.ending.is_none())
            || self.entry.is_some()
            || (self.restore && !self.ownership_known)
            || self
                .owner
                .as_ref()
                .is_some_and(|o| o.claimed.lock().unwrap().is_some() || o.adoption.lock().unwrap().is_some())
    }

    /// Takes a mirror report. A `Gone` ending forgets the record only when the link said the terminal no longer
    /// exists; any other (a refused connection or Hello, a protocol refusal, a host key that no longer matches) leaves
    /// the entry and its shell recorded, and Enter attaches it again.
    pub(crate) fn report(&mut self, report: MirrorReport, now: Instant) {
        if matches!(report.ended, Some(MirrorEnd::Replaced)) {
            self.replaced();
        }
        self.model.report(report, now);
        if let Some(Ending::Ended(MirrorEnd::Gone(why))) = &self.model.ending {
            if !self.answer.as_ref().is_some_and(OpenAnswer::terminal_gone) {
                self.model.ending = Some(Ending::Unreachable(why.clone()));
            }
        }
    }

    pub(crate) fn replaced(&mut self) {
        self.retry_entry = self.entry.take();
        if let Some(owner) = &self.owner {
            owner.claimed.lock().unwrap().take();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        self.again = self.again.or_else(|| self.model.again());
        self.starting = false;
        self.model = Model::new(self.registry.origin().remote().map(|p| p.host.as_str()).unwrap_or(""));
    }

    pub(crate) fn may_have_job(&self) -> bool {
        !self.ownership_known || self.starting || self.entry.is_some()
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(token) = self.listener.take() {
            self.registry.listeners.borrow_mut().remove(&token);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Again {
    Open,
    Attach,
}

impl Owner {
    pub(crate) fn saved(&self) -> io::Result<Option<TerminalEntry>> {
        Ok(self
            .store
            .terminals()
            .restored(
                &self.project.host_key_sha256,
                &self.project.root,
                self.slot,
                std::slice::from_ref(&self.module),
            )?
            .into_iter()
            .next())
    }

    /// The intent is durable before the link can send OpenTerminal. No shell environment crosses this seam.
    pub(crate) fn start(
        &self,
        connection: &RemoteConnection,
        cwd: PathBuf,
        again: Option<Again>,
        retry: Option<TerminalEntry>,
        size: PtySize,
        colors: TerminalColors,
        focused: bool,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<(TerminalSession, TerminalEntry, OpenAnswer)> {
        let client = connection
            .open_for(&self.project.host, &self.project.host_key_sha256)
            .ok_or_else(|| io::Error::other("the terminal connection belongs to another server"))?;
        self.start_with(
            connection,
            client.daemon(),
            &cwd,
            again,
            retry,
            MirrorConfig { size, colors, focused },
            wake,
            |options| {
                let link = connection.terminal_link(options, self.facts.clone())?;
                let answer = link.answer_handle();
                Ok((Box::new(link) as Box<dyn TermLink>, answer))
            },
        )
    }

    /// [`Self::start`] with the link made by `link` from the durable entry's options.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_with<'a>(
        &self,
        source: impl Into<eitri_core::remote_project::terminals::OpenSource<'a>>,
        installation: &str,
        cwd: &std::path::Path,
        again: Option<Again>,
        retry: Option<TerminalEntry>,
        config: MirrorConfig,
        wake: impl Fn() + Send + 'static,
        link: impl FnOnce(TerminalLinkOptions) -> io::Result<(Box<dyn TermLink>, OpenAnswer)>,
    ) -> io::Result<(TerminalSession, TerminalEntry, OpenAnswer)> {
        let (entry, options) = self.options(source, installation, cwd, again, retry, config.size, config.colors)?;
        let (link, answer) = link(options)?;
        let session = TerminalSession::mirror(link, config, wake)?;
        Ok((session, entry, answer))
    }

    /// Selects only this binding's restored, started or explicitly adopted identity, before constructing a link.
    pub(crate) fn options<'a>(
        &self,
        source: impl Into<eitri_core::remote_project::terminals::OpenSource<'a>>,
        installation: &str,
        cwd: &std::path::Path,
        again: Option<Again>,
        retry: Option<TerminalEntry>,
        size: PtySize,
        colors: TerminalColors,
    ) -> io::Result<(TerminalEntry, TerminalLinkOptions)> {
        let records = self.store.terminals();
        let key = &self.project.host_key_sha256;
        let root = &self.project.root;
        // These locks protect only in-memory identities; GTK must never wait for record or network work.
        let mut owned = self.claimed.lock().unwrap().clone();
        let adoption = self.adoption.lock().unwrap().clone();
        if again == Some(Again::Open) {
            if let Some(entry) = owned.as_ref() {
                records.forget(key, root, &entry.command_id)?;
            }
            owned = None;
            self.claimed.lock().unwrap().take();
        }
        let reclaimed = match owned.as_ref().or(retry.as_ref()) {
            Some(entry) => match records.claim(key, root, &entry.command_id, self.slot, &self.module) {
                // The record no longer holds it (`eitri host stop` cleared this host's records): nothing on the
                // server is this pane's any more, so the start opens a new shell instead of failing the same way
                // at every Enter.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.claimed.lock().unwrap().take();
                    None
                }
                claimed => Some(claimed?),
            },
            None => None,
        };
        let entry = if let Some(entry) = reclaimed {
            entry
        } else if let Some(command) = adoption.as_ref() {
            records.claim(key, root, command, self.slot, &self.module)?
        } else {
            // A reused number is not permission to attach an unclaimed shell, including an unanswered open.
            if self.saved()?.is_some() {
                return Err(io::Error::other(UNCLAIMED_REFUSAL));
            }
            records.begin_open(source, root, self.slot, &self.module, installation, now())?
        };
        self.adoption.lock().unwrap().take();
        *self.claimed.lock().unwrap() = Some(entry.clone());
        let options = TerminalLinkOptions {
            installation: entry.installation.clone(),
            target: entry.target(cwd)?,
            size,
            colors,
        };
        Ok((entry, options))
    }

    pub(crate) fn restore(&self) -> io::Result<Option<TerminalEntry>> {
        let entry = self.saved()?;
        *self.claimed.lock().unwrap() = entry.clone();
        Ok(entry)
    }

    pub(crate) fn opened(&self, entry: &TerminalEntry, identity: &(String, String, String)) -> io::Result<()> {
        self.store.terminals().opened(
            &self.project.host_key_sha256,
            &self.project.root,
            &entry.command_id,
            &identity.0,
            &identity.1,
            &identity.2,
        )?;
        if let Some(entry) = self.claimed.lock().unwrap().as_mut() {
            entry.terminal_id = Some(identity.0.clone());
            entry.daemon_instance = Some(identity.1.clone());
            entry.boot_id = Some(identity.2.clone());
            entry.state = eitri_core::remote_project::terminals::TerminalState::Open;
        }
        Ok(())
    }

    /// Answers the server's warning when it could not confirm the closed terminal's processes gone.
    pub(crate) fn close(&self, by_hand: bool, connection: Option<RemoteConnection>) -> io::Result<Option<String>> {
        if !by_hand {
            return Ok(None);
        }
        self.adoption.lock().unwrap().take();
        let entry = self.claimed.lock().unwrap().take();
        let Some(entry) = entry else {
            return Ok(None);
        };
        let mut control = connection;
        self.store.terminals().close(
            &self.project.host_key_sha256,
            &self.project.root,
            &entry.command_id,
            true,
            control
                .as_mut()
                .map(|c| c as &mut dyn eitri_core::remote_project::terminals::TerminalControl),
        )
    }

    pub(crate) fn orphans(&self, modules: &[String], other: Option<&[String]>) -> io::Result<Vec<TerminalEntry>> {
        let record = self
            .store
            .terminals()
            .record(&self.project.host_key_sha256, &self.project.root)?;
        let claims = orphan_claims(self.slot, modules, other, &record.entries);
        self.store
            .terminals()
            .orphans(&self.project.host_key_sha256, &self.project.root, &claims)
    }

    /// Answers the servers' cleanup warnings (one per stop that had one) and the failures, each kept whatever the
    /// other stops did.
    pub(crate) fn stop_orphans(
        &self,
        entries: &[TerminalEntry],
        control: &mut dyn eitri_core::remote_project::terminals::TerminalControl,
    ) -> (Vec<String>, Vec<String>) {
        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries {
            match self.store.terminals().close(
                &self.project.host_key_sha256,
                &self.project.root,
                &entry.command_id,
                true,
                Some(&mut *control),
            ) {
                Ok(warning) => warnings.extend(warning),
                Err(error) => failures.push(error.to_string()),
            }
        }
        (warnings, failures)
    }

    pub(crate) fn show_gone(&self, entry: &TerminalEntry) -> io::Result<()> {
        self.store.terminals().show_gone(
            &self.project.host_key_sha256,
            &self.project.root,
            &entry.command_id,
            || {},
        )?;
        let mut claimed = self.claimed.lock().unwrap();
        if claimed
            .as_ref()
            .is_some_and(|owned| owned.command_id == entry.command_id)
        {
            claimed.take();
        }
        Ok(())
    }
}

/// Unknown peer modules protect every entry in that slot until its live claims are known.
fn orphan_claims(
    slot: u8,
    modules: &[String],
    other: Option<&[String]>,
    entries: &[TerminalEntry],
) -> Vec<(u8, String)> {
    let peer = if slot == 1 { 2 } else { 1 };
    let mut claims: Vec<_> = modules.iter().cloned().map(|module| (slot, module)).collect();
    claims.extend(match other {
        Some(modules) => modules.iter().cloned().map(|module| (peer, module)).collect::<Vec<_>>(),
        None => entries
            .iter()
            .filter(|entry| entry.slot == peer)
            .map(|entry| (peer, entry.module.clone()))
            .collect(),
    });
    claims
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct Model {
    host: String,
    link: LinkState,
    synced: bool,
    waiting: Option<WaitWhy>,
    pub(crate) recorded: bool,
    pub(crate) opening: bool,
    refused_until: Option<Instant>,
    lost: u64,
    pub(crate) ending: Option<Ending>,
    pub(crate) history: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    Exited(ExitInfo),
    Ended(MirrorEnd),
    Failed(String),
    /// The link ended without the terminal being gone: its entry stays and Enter attaches it again.
    Unreachable(String),
}

impl Model {
    pub(crate) fn new(host: &str) -> Self {
        Self {
            host: display_text(host),
            link: LinkState::Connecting,
            synced: false,
            waiting: None,
            recorded: false,
            opening: false,
            refused_until: None,
            lost: 0,
            ending: None,
            history: None,
        }
    }

    pub(crate) fn report(&mut self, report: MirrorReport, now: Instant) {
        if let Some(link) = report.link {
            if link != LinkState::Live {
                self.synced = false;
                self.waiting = None;
            }
            self.link = link;
        }
        if let Some(history) = report.synced {
            self.synced = true;
            self.waiting = None;
            self.history = history_line(history);
        }
        // A snapshot clears the wait when applied, so a wait merged with it can be newer.
        if let Some(waiting) = report.waiting {
            self.waiting = waiting;
        }
        if report.refused_input_bytes > 0 {
            self.refused_until = Some(now + Duration::from_secs(3));
        }
        if report.lost_input_bytes > 0 {
            self.lost = report.lost_input_bytes;
        }
        if let Some(end) = report.ended {
            self.ending = Some(Ending::Ended(end));
        }
    }

    pub(crate) fn input(&mut self, now: Instant) -> bool {
        self.lost = 0;
        let allowed = self.recorded
            && self.link == LinkState::Live
            && (self.synced || self.waiting.is_some())
            && self.ending.is_none();
        if !allowed {
            self.refused_until = Some(now + Duration::from_secs(3));
        }
        allowed
    }

    pub(crate) fn overlay(&self, now: Instant) -> Option<String> {
        if self.ending.is_some() {
            return None;
        }
        let host = &self.host;
        if self.refused_until.is_some_and(|until| now < until) {
            return Some(if !self.recorded && (self.opening || self.link == LinkState::Live) {
                format!("keys not sent: starting the terminal on {host}")
            } else {
                format!("keys not sent: not connected to {host}")
            });
        }
        if self.lost > 0 {
            return Some(format!(
                "{} bytes typed before the link dropped did not reach {host}",
                self.lost
            ));
        }
        match self.link {
            LinkState::Connecting => Some(format!("the terminal runs on {host}: connecting…")),
            LinkState::Disconnected { .. } => Some(format!("disconnected from {host} — reconnecting…")),
            LinkState::Reconnecting { attempt, next_in_ms } => Some(format!(
                "disconnected from {host} — reconnecting in {} s (attempt {attempt})",
                next_in_ms.div_ceil(1000)
            )),
            LinkState::Live => match self.waiting {
                Some(WaitWhy::OpenSequence) => Some(format!(
                    "waiting for the program on {host} to finish an escape sequence"
                )),
                Some(WaitWhy::TooDeep) => {
                    Some("this terminal's state cannot be copied yet (keyboard modes nested too deep)".into())
                }
                Some(WaitWhy::SnapshotTooLarge) => {
                    Some("this terminal's state is too large to copy; reduce its size".into())
                }
                None if !self.synced || !self.recorded => Some(format!("catching up with {host}…")),
                None => None,
            },
        }
    }

    pub(crate) fn expiry(&self) -> Option<Instant> {
        self.refused_until
    }

    pub(crate) fn notice(&self) -> Option<String> {
        let host = &self.host;
        Some(match self.ending.as_ref()? {
            Ending::Failed(why) => format!(
                "[could not start the terminal on {host}: {} — Enter reconnects]",
                display_text(why)
            ),
            Ending::Exited(exit) => {
                let status = exit
                    .code
                    .map(|code| code.to_string())
                    .or_else(|| exit.signal.map(|signal| format!("signal {signal}")))
                    .unwrap_or_else(|| "with unknown status".into());
                format!("[process exited {status} on {host} — Enter starts a new shell]")
            }
            Ending::Unreachable(why) => format!(
                "[could not reach the terminal on {host}: {} — Enter tries again]",
                display_text(why)
            ),
            Ending::Ended(MirrorEnd::Gone(why)) => format!(
                "[Enter starts a new shell — the terminal on {host} is gone: {}]",
                display_text(why)
            ),
            Ending::Ended(MirrorEnd::Closed) => {
                format!("[the terminal on {host} was closed — Enter starts a new shell]")
            }
            Ending::Ended(MirrorEnd::Stopped) => {
                format!("[the terminal on {host} was stopped (eitri host stop) — Enter starts a new shell]")
            }
            Ending::Ended(MirrorEnd::Replaced) => {
                "[this terminal was opened in another window — Enter takes it back]".into()
            }
            Ending::Ended(MirrorEnd::Refused(why)) => {
                format!("[the server sent a terminal record this window cannot use ({why}) — Enter reconnects]")
            }
        })
    }

    pub(crate) fn ending_row(&self, cursor_row: Option<u16>, rows: u16) -> u16 {
        let row = cursor_row.unwrap_or(rows.saturating_sub(1));
        if matches!(self.ending, Some(Ending::Exited(_))) {
            row
        } else {
            row.saturating_add(1)
        }
        .min(rows.saturating_sub(1))
    }

    pub(crate) fn again(&self) -> Option<Again> {
        Some(match self.ending.as_ref()? {
            Ending::Ended(MirrorEnd::Replaced | MirrorEnd::Refused(_)) | Ending::Failed(_) | Ending::Unreachable(_) => {
                Again::Attach
            }
            _ => Again::Open,
        })
    }
}

fn history_line(note: HistoryNote) -> Option<String> {
    if note.trimmed > 0 {
        Some(format!("[{} older lines are not kept]", note.trimmed))
    } else if note.older_output_gone {
        Some("[older output from while you were away is beyond the 10,000-line history]".into())
    } else {
        None
    }
}

pub(crate) fn identity(generation: u64, id: &str) -> Option<ShellIdentity> {
    Some(ShellIdentity::Server {
        generation,
        terminal_id: u128::from_str_radix(id, 16).ok()?,
    })
}

/// Uses the terminal's indicator operation and colours, with pane-owned wording and position.
pub(crate) fn notice_ops(text: &str, row: u16, cols: u16, colors: TerminalColors) -> Vec<PaintOp> {
    let mut ops = eitri_terminal::indicator_ops(0, 0, cols, &terminal_render::Palette::xterm_default());
    if let Some(PaintOp::DrawNotice {
        row: r,
        col,
        cols: width,
        text: line,
        color,
        background,
    }) = ops.first_mut()
    {
        *r = row;
        *col = 0;
        *width = cols;
        *line = text.chars().take(cols as usize).collect();
        *color = colors.background;
        *background = colors.foreground;
    }
    ops
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub fn project() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "installation".into(),
            sync: None,
            created_at: 1,
            queued_stops: vec![],
        }
    }

    pub fn registry(slot: u8) -> Rc<Registry> {
        let dir = agent::state_dirs::test_workspace_dir("terminal-pane-binding");
        window(
            WindowOrigin::Remote(project()),
            Some(slot),
            Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None)),
        )
    }

    /// A chat or page window of the same project, reading the same terminal record as `sharing`.
    pub fn module_registry(sharing: &Registry) -> Rc<Registry> {
        window(WindowOrigin::Remote(project()), None, sharing.store.clone())
    }

    /// A full window of the project deciding its slot as `prepare` does, reading the record `sharing` reads (a fresh
    /// one without it).
    pub fn full_window(sharing: Option<&Registry>) -> Rc<Registry> {
        let store = sharing.map(|other| other.store.clone()).unwrap_or_else(|| {
            let dir = agent::state_dirs::test_workspace_dir("terminal-pane-binding");
            Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None))
        });
        let mut registry = Registry::new(WindowOrigin::Remote(project()), None, Rc::new(|_| {}));
        registry.store = store;
        registry.claim_slot(TerminalWindow::Full);
        let registry = Rc::new(registry);
        WINDOW.with(|window| *window.borrow_mut() = Some(registry.clone()));
        registry
    }

    /// This thread's window is a local project's.
    pub fn local_window() -> Rc<Registry> {
        let dir = agent::state_dirs::test_workspace_dir("terminal-pane-binding");
        window(
            WindowOrigin::Local,
            Some(1),
            Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None)),
        )
    }

    fn window(origin: WindowOrigin, slot: Option<u8>, store: Arc<RemoteProjects>) -> Rc<Registry> {
        let mut registry = Registry::new(origin, slot, Rc::new(|_| {}));
        registry.store = store;
        let registry = Rc::new(registry);
        WINDOW.with(|window| *window.borrow_mut() = Some(registry.clone()));
        registry
    }

    pub fn connection() -> RemoteConnection {
        struct Peer;
        impl agent::ssh::ChannelLauncher for Peer {
            fn spawn(&self, _: &[std::ffi::OsString]) -> io::Result<std::process::Child> {
                std::process::Command::new("python3")
                    .args([
                        "-c",
                        r#"
import sys
i, o = sys.stdin.buffer, sys.stdout.buffer
o.write(bytes([0, 0, 0, 4, 0, 0, 0, 0, 0])); o.flush()
if len(i.read(24)) != 24: sys.exit(0)
while True:
    h = i.read(9)
    if len(h) != 9: break
    i.read(int.from_bytes(h[:3], 'big'))
    if h[3] == 1: break
"#,
                    ])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            }
        }
        let p = project();
        let client = agent::remote_client::HelperClient::connect_daemon(
            Arc::new(Peer),
            &p.host,
            Some(&p.host_key_sha256),
            "installation",
        )
        .unwrap();
        RemoteConnection::with_client(client, p, "sidecar".into(), "a".repeat(64))
    }

    pub fn hand_off(registry: &Registry, connection: RemoteConnection) -> bool {
        registry.hand_off(connection.project().clone(), connection)
    }

    pub fn reject_hand_off(registry: &Registry, project: RemoteProject, connection: RemoteConnection) -> bool {
        registry.hand_off(project, connection)
    }
    pub fn records(registry: &Registry) -> Arc<RemoteProjects> {
        registry.store.clone()
    }
    pub fn listeners(registry: &Registry) -> usize {
        registry.listeners.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.3.2".into(),
            sync: None,
            created_at: 1,
            queued_stops: vec![],
        }
    }

    #[test]
    fn a_remote_origin_never_spawns_a_local_shell() {
        let origin = WindowOrigin::Remote(project());
        assert!(matches!(source_for(&origin), TermSource::Waiting));
        assert_eq!(
            Model::new("example-host").overlay(Instant::now()).unwrap(),
            "the terminal runs on example-host: connecting…"
        );
        let pane = eitri_core::source_scan::read_required("shell/src/terminal/pane.rs");
        let code: String = eitri_core::source_scan::code_only(&pane.text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let local = code.find("TermSource::Local=>").expect("local spawn branch");
        let spawn = code.find("TerminalSession::spawn(").expect("local session");
        assert!(local < spawn);
        let remote_branches = &code[code.find("matchsource{").unwrap()..local];
        assert!(remote_branches.contains("TermSource::Waiting=>"));
        assert!(remote_branches.contains("TermSource::Server(connection)=>"));
        assert_eq!(remote_branches.matches("return;").count(), 2);
        assert_eq!(code.matches("TerminalSession::spawn(").count(), 1);
    }

    #[test]
    fn a_local_copy_keeps_its_local_shell() {
        for origin in [
            WindowOrigin::Local,
            WindowOrigin::LocalCopy {
                host: "example-host".into(),
            },
        ] {
            assert!(matches!(source_for(&origin), TermSource::Local));
        }
    }

    #[test]
    fn an_accepted_handoff_follows_key_changes_and_rejects_other_hosts() {
        let registry = Registry::new(WindowOrigin::Remote(project()), Some(1), Rc::new(|_| {}));
        let mut changed = project();
        changed.host_key_sha256 = "SHA256:new".into();
        assert!(registry.hand_off(changed.clone(), 42));
        assert_eq!(*registry.origin.borrow(), WindowOrigin::Remote(changed));
        let mut other = project();
        other.host = "another-host".into();
        assert!(!registry.hand_off(other, 43));
        assert_eq!(*registry.connection.borrow(), Some(42));
    }

    #[test]
    fn a_wait_after_a_snapshot_in_the_same_report_stays_visible() {
        let now = Instant::now();
        let mut model = Model::new("example-host");
        model.recorded = true;
        model.report(
            MirrorReport {
                link: Some(LinkState::Live),
                synced: Some(HistoryNote::default()),
                waiting: Some(Some(WaitWhy::OpenSequence)),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            model.overlay(now).as_deref(),
            Some("waiting for the program on example-host to finish an escape sequence")
        );
        assert!(model.synced);
        assert!(model.input(now));
    }

    #[test]
    fn overlay_lines_follow_link_states() {
        let now = Instant::now();
        let mut model = Model::new("example-host");
        for (link, expected) in [
            (LinkState::Connecting, "connecting…"),
            (
                LinkState::Reconnecting {
                    attempt: 4,
                    next_in_ms: 8000,
                },
                "reconnecting in 8 s (attempt 4)",
            ),
            (LinkState::Live, "catching up with example-host…"),
        ] {
            model.report(
                MirrorReport {
                    link: Some(link),
                    ..Default::default()
                },
                now,
            );
            assert!(model.overlay(now).unwrap().contains(expected));
        }
        assert!(!model.input(now));
        assert_eq!(
            model.overlay(now).unwrap(),
            "keys not sent: starting the terminal on example-host"
        );
        for why in [WaitWhy::OpenSequence, WaitWhy::TooDeep, WaitWhy::SnapshotTooLarge] {
            model.report(
                MirrorReport {
                    waiting: Some(Some(why)),
                    ..Default::default()
                },
                now,
            );
            assert!(model.overlay(now + Duration::from_secs(4)).is_some());
        }
        model.recorded = true;
        model.report(
            MirrorReport {
                synced: Some(HistoryNote::default()),
                ..Default::default()
            },
            now,
        );
        assert!(model.overlay(now + Duration::from_secs(4)).is_none());
        model.report(
            MirrorReport {
                lost_input_bytes: 12,
                ..Default::default()
            },
            now,
        );
        assert!(model
            .overlay(now + Duration::from_secs(4))
            .unwrap()
            .starts_with("12 bytes typed"));
        assert!(model.input(now + Duration::from_secs(4)));
        assert!(model.overlay(now + Duration::from_secs(4)).is_none());
        model.report(
            MirrorReport {
                link: Some(LinkState::Disconnected { since_ms: 0 }),
                ..Default::default()
            },
            now,
        );
        assert!(!model.input(now));
        assert_eq!(
            model.overlay(now).unwrap(),
            "keys not sent: not connected to example-host"
        );
        assert!(model
            .overlay(now + Duration::from_secs(4))
            .unwrap()
            .starts_with("disconnected"));
    }

    fn owner(label: &str) -> Owner {
        let dir = agent::state_dirs::test_workspace_dir(label);
        Owner {
            project: project(),
            slot: 1,
            module: "terminal".into(),
            store: Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None)),
            facts: TerminalFactsHandle::default(),
            claimed: Default::default(),
            adoption: Default::default(),
        }
    }

    #[test]
    fn unknown_peer_claims_are_never_offered_and_known_peer_claims_are_respected() {
        let owner = owner("terminal-orphans");
        let records = owner.store.terminals();
        for (slot, module) in [(1, "terminal"), (1, "terminal#2"), (2, "terminal"), (2, "terminal#2")] {
            records
                .begin_open(
                    &owner.project.host_key_sha256,
                    &owner.project.root,
                    slot,
                    module,
                    "installation",
                    1,
                )
                .unwrap();
        }
        let mine = vec!["terminal".into()];
        let unknown = owner.orphans(&mine, None).unwrap();
        assert_eq!(unknown.len(), 1);
        assert_eq!((unknown[0].slot, unknown[0].module.as_str()), (1, "terminal#2"));
        let known = owner.orphans(&mine, Some(&mine)).unwrap();
        assert_eq!(known.len(), 2);
        let absent = owner.orphans(&mine, Some(&[])).unwrap();
        assert_eq!(absent.len(), 3);
        let claimed = records
            .claim(
                &owner.project.host_key_sha256,
                &owner.project.root,
                &unknown[0].command_id,
                1,
                "terminal#3",
            )
            .unwrap();
        assert_eq!(claimed.module, "terminal#3");
        assert_eq!(claimed.command_id, unknown[0].command_id);
    }

    #[test]
    fn an_in_flight_binding_claims_its_module_before_the_offer_reads_the_record() {
        for phase in ["starting", "opening", "entry"] {
            let registry = test_support::registry(1);
            let mut binding = Binding::new(registry.clone(), "terminal".into());
            binding.ownership_known = true;
            let owner = binding.current_owner().unwrap();
            let records = owner.store.terminals();
            let entry = records
                .begin_open(
                    &owner.project.host_key_sha256,
                    &owner.project.root,
                    1,
                    "terminal",
                    "installation",
                    1,
                )
                .unwrap();
            match phase {
                "starting" => binding.starting = true,
                "opening" => binding.model.opening = true,
                "entry" => binding.entry = Some(entry),
                _ => unreachable!(),
            }
            assert!(binding.claims_module(), "{phase} must claim its module");
            let modules = binding
                .claims_module()
                .then(|| "terminal".to_owned())
                .into_iter()
                .collect::<Vec<_>>();
            assert!(owner.orphans(&modules, Some(&[])).unwrap().is_empty(), "{phase}");
        }
    }

    #[test]
    fn a_refused_unclaimed_open_leaves_its_terminal_available_for_the_offer() {
        let registry = test_support::registry(1);
        let mut binding = Binding::new(registry, "terminal".into());
        binding.ownership_known = true;
        binding.model.opening = true;
        binding.model.ending = Some(Ending::Failed(UNCLAIMED_REFUSAL.into()));
        assert!(!binding.claims_module());
    }

    #[test]
    fn a_gone_notice_keeps_the_restart_action_visible_in_narrow_panes() {
        let mut model = Model::new("example-host");
        model.ending = Some(Ending::Ended(MirrorEnd::Gone(
            "the terminal is no longer recorded after the server restarted".into(),
        )));
        let notice = model.notice().unwrap();
        assert!(notice.contains("the terminal is no longer recorded after the server restarted"));
        for cols in [80, 40] {
            let ops = notice_ops(&notice, 9, cols, TerminalColors::default());
            let PaintOp::DrawNotice {
                text, row, cols: width, ..
            } = &ops[0]
            else {
                panic!("the ending must draw a notice");
            };
            assert!(text.contains("Enter starts a new shell"), "{cols} columns: {text}");
            assert_eq!((*row, *width), (9, cols));
        }
    }

    #[test]
    fn ending_indicators_escape_values_and_follow_the_last_screen_cursor() {
        let mut model = Model::new("example-host\u{202e}");
        model.ending = Some(Ending::Exited(ExitInfo {
            code: Some(0),
            signal: None,
            detached: false,
        }));
        let notice = model.notice().unwrap();
        assert!(notice.contains("process exited 0 on example-host\\u{202e}"));
        assert_eq!(model.ending_row(Some(5), 10), 5);
        let ops = notice_ops(&notice, model.ending_row(Some(9), 10), 100, TerminalColors::default());
        assert!(matches!(&ops[0], PaintOp::DrawNotice { row: 9, .. }));
        assert!(ops.iter().all(|op| op.layer() == terminal_render::PaintLayer::Overlay));
        model.ending = Some(Ending::Ended(MirrorEnd::Replaced));
        assert_eq!(model.ending_row(Some(5), 10), 6);
        assert_eq!(model.ending_row(Some(9), 10), 9);
    }

    #[test]
    fn record_work_stays_ordered_when_a_close_follows_a_slow_open() {
        let work = Work::default();
        let (release, waiting) = std::sync::mpsc::channel();
        let (tell, told) = std::sync::mpsc::channel();
        let first = tell.clone();
        work.run(move || {
            waiting.recv().unwrap();
            first.send("opened").unwrap();
        });
        work.run(move || {
            tell.send("closed").unwrap();
        });
        assert!(told.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(told.recv_timeout(Duration::from_secs(10)).unwrap(), "opened");
        assert_eq!(told.recv_timeout(Duration::from_secs(10)).unwrap(), "closed");
    }
    #[test]
    fn an_open_holds_keys_until_the_answer_is_durable_even_after_a_snapshot() {
        let now = Instant::now();
        let mut model = Model::new("example-host");
        model.opening = true;
        assert!(!model.input(now));
        assert_eq!(
            model.overlay(now).unwrap(),
            "keys not sent: starting the terminal on example-host"
        );
        model.report(
            MirrorReport {
                link: Some(LinkState::Live),
                synced: Some(HistoryNote::default()),
                ..Default::default()
            },
            now,
        );
        assert!(!model.input(now));
        model.recorded = true;
        assert!(model.input(now + Duration::from_secs(4)));
    }
    #[test]
    fn a_binding_that_never_owned_an_entry_stops_nothing() {
        let owner = owner("terminal-unowned-close");
        let entry = owner
            .store
            .terminals()
            .begin_open(
                &owner.project.host_key_sha256,
                &owner.project.root,
                1,
                "terminal",
                "installation",
                1,
            )
            .unwrap();
        owner.close(true, None).unwrap();
        assert_eq!(owner.saved().unwrap(), Some(entry));
    }

    #[test]
    fn an_unshown_restored_terminal_with_unknown_jobs_still_asks_on_close() {
        let registry = Rc::new(Registry::new(WindowOrigin::Remote(project()), Some(1), Rc::new(|_| {})));
        let mut binding = Binding::new(registry, "terminal".into());
        assert!(binding.may_have_job(), "the ownership read has not answered");
        binding.ownership_known = true;
        assert!(!binding.may_have_job(), "an absent ownership entry has no server job");
        let owner = owner("terminal-hidden-restore");
        binding.entry = Some(
            owner
                .store
                .terminals()
                .begin_open(
                    &owner.project.host_key_sha256,
                    &owner.project.root,
                    1,
                    "terminal",
                    "installation",
                    1,
                )
                .unwrap(),
        );
        assert!(
            binding.may_have_job(),
            "a saved terminal counts before any mirror is started"
        );
    }

    #[test]
    fn a_second_full_window_of_a_server_project_owns_no_slot_while_the_first_runs() {
        let dir = agent::state_dirs::test_workspace_dir("terminal-first-slot");
        let store = Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None));
        let window = |origin: WindowOrigin, kind: TerminalWindow| {
            let mut registry = Registry::<RemoteConnection>::new(origin, None, Rc::new(|_| {}));
            registry.store = store.clone();
            registry.claim_slot(kind);
            registry
        };
        let first = window(WindowOrigin::Remote(project()), TerminalWindow::Full);
        assert_eq!(first.slot(), Some(1));
        // `--new-window`, or a window that found the path's pair lock held by another origin's window: unpaired.
        let unpaired = window(WindowOrigin::Remote(project()), TerminalWindow::Full);
        assert_eq!(unpaired.slot(), None);
        assert_eq!(unpaired.no_slot_reason(), ANOTHER_WINDOW_OWNS);
        assert!(
            unpaired.owner("terminal".into()).is_none(),
            "it restores, stops and offers nothing"
        );
        // Window 2 of the first window, a window with no pair lock at all, and another server's window keep theirs.
        assert_eq!(
            window(WindowOrigin::Remote(project()), TerminalWindow::Second).slot(),
            Some(2)
        );
        // A window with no pair lock to take still finds the first slot's lock held.
        assert_eq!(
            window(WindowOrigin::Remote(project()), TerminalWindow::Alone).slot(),
            None
        );
        let mut other = project();
        other.host_key_sha256 = "SHA256:other".into();
        assert_eq!(
            window(WindowOrigin::Remote(other), TerminalWindow::Full).slot(),
            Some(1)
        );
        assert_eq!(window(WindowOrigin::Local, TerminalWindow::Full).slot(), Some(1));
        assert_eq!(
            window(WindowOrigin::Remote(project()), TerminalWindow::Chat).slot(),
            None
        );
        // The lock is the first window's for its life only.
        drop(first);
        drop(unpaired);
        assert_eq!(
            window(WindowOrigin::Remote(project()), TerminalWindow::Full).slot(),
            Some(1)
        );
        // Record scans skip the lock file.
        let records = store.terminals();
        records
            .begin_open(
                "SHA256:example",
                std::path::Path::new("/srv/project"),
                1,
                "terminal",
                "installation",
                1,
            )
            .unwrap();
        assert_eq!(
            records
                .record("SHA256:example", std::path::Path::new("/srv/project"))
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(records.clear_host("SHA256:example").unwrap().is_empty());
    }

    #[test]
    fn a_window_that_lost_the_first_slot_takes_it_once_its_owner_goes_and_never_beside_it() {
        let dir = agent::state_dirs::test_workspace_dir("terminal-first-slot-retry");
        let store = Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None));
        let window = |kind: TerminalWindow| {
            let mut registry =
                Registry::<RemoteConnection>::new(WindowOrigin::Remote(project()), None, Rc::new(|_| {}));
            registry.store = store.clone();
            registry.claim_slot(kind);
            registry
        };
        // The previous window 1 is still stopping its terminals when the project opens again.
        let exiting = window(TerminalWindow::Full);
        let late = window(TerminalWindow::Full);
        let alone = window(TerminalWindow::Alone);
        for waiting in [&late, &alone] {
            assert!(!waiting.retry_slot(), "the owner still runs");
            assert!(
                waiting.owner("terminal".into()).is_none(),
                "no slot-1 record is touched"
            );
            assert_eq!(waiting.no_slot_reason(), ANOTHER_WINDOW_OWNS);
        }
        drop(exiting);
        // Both try; the lock lets exactly one through.
        assert!(late.retry_slot());
        assert!(!alone.retry_slot());
        assert_eq!((late.slot(), alone.slot()), (Some(1), None));
        assert!(late.owner("terminal".into()).is_some());
        assert!(!late.retry_slot(), "an owner does not take it again");
        assert!(!window(TerminalWindow::Chat).retry_slot(), "a chat window never tries");
        drop(late);
        assert!(alone.retry_slot());
    }

    #[test]
    fn only_an_unopenable_lock_file_gives_a_window_with_no_pair_lock_the_first_slot_blind() {
        let dir = agent::state_dirs::test_workspace_dir("terminal-first-slot-error");
        // State under a regular file: neither the record directory nor the lock file can be made.
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let store = Arc::new(RemoteProjects::new(Some(blocker.as_os_str()), None));
        let window = |kind: TerminalWindow| {
            let mut registry =
                Registry::<RemoteConnection>::new(WindowOrigin::Remote(project()), None, Rc::new(|_| {}));
            registry.store = store.clone();
            registry.claim_slot(kind);
            registry
        };
        assert_eq!(window(TerminalWindow::Alone).slot(), Some(1));
        let full = window(TerminalWindow::Full);
        assert_eq!(full.slot(), None);
        assert_eq!(full.no_slot_reason(), SLOT_LOCK_FAILED);
    }

    /// Windows of `project()` (with `key`) over one record directory, deciding their slot as `prepare` does.
    fn slot_windows() -> (Arc<RemoteProjects>, impl Fn(TerminalWindow, &str) -> Registry<i32>) {
        let dir = agent::state_dirs::test_workspace_dir("terminal-first-slot-key");
        let store = Arc::new(RemoteProjects::new(Some(dir.as_os_str()), None));
        // The record directory exists, private, as after any terminal of this project.
        store
            .terminals()
            .begin_open(
                "SHA256:example",
                std::path::Path::new("/srv/project"),
                1,
                "seed",
                "installation",
                1,
            )
            .unwrap();
        let shared = store.clone();
        (store, move |kind: TerminalWindow, key: &str| {
            let mut project = project();
            project.host_key_sha256 = key.into();
            let mut registry = Registry::<i32>::new(WindowOrigin::Remote(project), None, Rc::new(|_| {}));
            registry.store = shared.clone();
            registry.claim_slot(kind);
            registry
        })
    }

    /// Makes the lock file impossible to open until the returned closure puts the directory back.
    fn break_record_dir(store: &RemoteProjects) -> impl FnOnce() {
        let records = store
            .terminals()
            .path("SHA256:example", std::path::Path::new("/srv/project"))
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let moved = records.with_extension("moved");
        std::fs::rename(&records, &moved).unwrap();
        std::fs::write(&records, b"").unwrap();
        move || {
            std::fs::remove_file(&records).unwrap();
            std::fs::rename(&moved, &records).unwrap();
        }
    }

    #[test]
    fn a_retry_that_cannot_try_the_lock_never_gives_the_slot() {
        let (store, window) = slot_windows();
        let holder = window(TerminalWindow::Full, "SHA256:example");
        for kind in [TerminalWindow::Alone, TerminalWindow::Full] {
            let waiting = window(kind, "SHA256:example");
            assert_eq!(waiting.slot(), None, "{kind:?} found the lock held");
            let mend = break_record_dir(&store);
            assert!(
                !waiting.retry_slot(),
                "{kind:?}: an error on a retry is not a free slot"
            );
            assert_eq!(waiting.slot(), None);
            mend();
        }
        drop(holder);
    }

    #[test]
    fn a_window_that_took_slot_one_blind_takes_the_lock_once_it_can() {
        let (store, window) = slot_windows();
        let mend = break_record_dir(&store);
        let alone = window(TerminalWindow::Alone, "SHA256:example");
        assert_eq!(alone.slot(), Some(1), "a first try that cannot open the lock file");
        assert!(!alone.retry_slot(), "still unable: it keeps slot 1");
        mend();
        assert!(!alone.retry_slot(), "its panes see no change");
        assert_eq!(alone.slot(), Some(1));
        // It holds the lock now: a full window opened later does not own slot 1 beside it.
        let later = window(TerminalWindow::Full, "SHA256:example");
        assert_eq!(later.slot(), None);
        assert_eq!(later.no_slot.get(), ANOTHER_WINDOW_OWNS);
    }

    #[test]
    fn a_slotless_window_retries_the_lock_of_the_key_it_started_with() {
        let (_store, window) = slot_windows();
        let holder = window(TerminalWindow::Full, "SHA256:example");
        let late = window(TerminalWindow::Full, "SHA256:example");
        assert_eq!(late.slot(), None);
        // The server's key changes; the window accepts it for the same host and root.
        let mut changed = project();
        changed.host_key_sha256 = "SHA256:changed".into();
        assert!(late.hand_off(changed, 1));
        assert!(
            !late.retry_slot(),
            "the old owner still holds the lock of the key both started with"
        );
        assert_eq!(late.slot(), None);
        // A window started after the change takes the new key's lock: the documented limit.
        assert_eq!(window(TerminalWindow::Full, "SHA256:changed").slot(), Some(1));
        drop(holder);
        assert!(late.retry_slot());
    }

    #[test]
    fn cleanup_warnings_reach_the_windows_toast_and_survive_a_failed_stop_beside_them() {
        use eitri_core::remote_project::terminals::{TerminalControl, CLOSE_WARNING_NOTICE};
        let band = Rc::new(RefCell::new(Vec::<String>::new()));
        let toast = Rc::new(RefCell::new(Vec::<String>::new()));
        let registry = Registry::<RemoteConnection>::new(WindowOrigin::Remote(project()), Some(1), {
            let band = band.clone();
            Rc::new(move |text| band.borrow_mut().push(text))
        });
        registry.set_warning({
            let toast = toast.clone();
            Rc::new(move |text| toast.borrow_mut().push(text))
        });
        registry.report_close(Ok(Some(CLOSE_WARNING_NOTICE.into())));
        assert_eq!(*toast.borrow(), [CLOSE_WARNING_NOTICE]);
        assert!(band.borrow().is_empty(), "the panel's band may be hidden or unloaded");
        registry.report_close(Err("refused".into()));
        assert_eq!(*band.borrow(), ["refused"]);

        struct Mixed;
        impl TerminalControl for Mixed {
            fn stop(&mut self, _: &std::path::Path, entry: &TerminalEntry) -> io::Result<Option<String>> {
                match entry.module.as_str() {
                    "warned" => Ok(Some(CLOSE_WARNING_NOTICE.into())),
                    _ => Err(io::Error::other("unreachable server")),
                }
            }
        }
        let owner = owner("terminal-stop-warnings");
        let entries: Vec<_> = ["warned", "failed"]
            .into_iter()
            .map(|module| {
                owner
                    .store
                    .terminals()
                    .begin_open(
                        &owner.project.host_key_sha256,
                        &owner.project.root,
                        1,
                        module,
                        "installation",
                        1,
                    )
                    .unwrap()
            })
            .collect();
        let answer = owner.stop_orphans(&entries, &mut Mixed);
        assert_eq!(answer.0, [CLOSE_WARNING_NOTICE]);
        assert_eq!(answer.1.len(), 1);
        registry.report_stops(answer);
        assert_eq!(toast.borrow().len(), 2);
        assert!(band.borrow()[1].starts_with("terminal stops: "));
    }

    #[test]
    fn an_unclaimed_refusal_can_bring_the_offer_back() {
        let registry = Registry::<RemoteConnection>::new(WindowOrigin::Remote(project()), Some(1), Rc::new(|_| {}));
        registry.offer_again();
        let runs = Rc::new(Cell::new(0));
        let counted = runs.clone();
        registry.set_offer(Rc::new(move || counted.set(counted.get() + 1)));
        registry.offer_again();
        registry.offer_again();
        assert_eq!(runs.get(), 2);
    }
}
