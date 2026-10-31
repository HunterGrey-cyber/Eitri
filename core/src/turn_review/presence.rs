//! Which Eitri windows have a project open, and which of them are running an agent turn, so a
//! revert in one window can refuse while another could be writing the same files.
//!
//! Each window keeps an exclusive `flock` on `windows/<pid>` for as long as it lives, and one on
//! `running/<pid>.<tab>` for as long as a tab's turn runs, both under the project's review
//! directory. A lock is released by the kernel when its process dies, so a killed window never
//! blocks anyone: a file nobody holds is a leftover, and whoever finds it removes it. Nothing here
//! is a lease or a timestamp, and nothing here is advisory to the window that holds it: the window
//! checks its own state before a revert too ([`PresenceGuard::check`]).
//!
//! The files are held by a thread of their own ([`PresenceHolder`]) so that the GTK thread never
//! touches the disk for them: it only tells the thread which tabs run a turn.
//!
//! **Linked windows are one presence.** Windows linked to each other each hold their own
//! `windows/<pid>`, but a window does not count a linked partner's as another window while the link
//! is up: [`PresenceGuard::set_partner`] names the one partner of a pair, and
//! [`PresenceGuard::add_partner`] and [`PresenceGuard::remove_partner`] keep the set of a window
//! with several links, each by pid and start time, and the check skips those files. A partner's
//! turns still count, through `running/`, which skips nothing but this process's own files. A
//! partner that said it cannot mark its turns as running sets its [`Partner::counts_as_other`],
//! and from then until it says it can again it is another window like any other. Writes are kept
//! one at a time across windows by the project's write slot ([`super::write_slot`]), which every
//! window takes, linked or not.
//!
//! **A group of windows is a star, and window 1 decides.** A module window is linked to window 1
//! alone, so every other module window's file would look like another window to it, and its own
//! partners say nothing about them. Only window 1 knows the whole group, so a module window's
//! [`PresenceGuard::check_own`] covers only itself and its revert asks window 1 through a
//! [`GroupGuard`]; window 1 answers with its own [`PresenceGuard::check`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::shadow::{flock_file, private_root, try_flock_existing, ShadowError, ShadowLock};
use super::write_slot::{SlotBusy, SlotHold, WriteSlot};

/// How often the holder checks that its files are still the ones the directory names.
const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// How long taking one of the holder's own locks may wait for someone probing the same file.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// How many times a lock is retaken because the path stopped naming the locked file meanwhile.
const LOCK_ATTEMPTS: usize = 8;

/// Why a revert is refused for what another window is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// A turn of another window is running.
    TurnRunning { pid: u32 },
    /// Another window has the project open.
    OtherWindow { pid: u32 },
}

impl Busy {
    pub fn reason(&self) -> &'static str {
        match self {
            Busy::TurnRunning { .. } => "an agent turn is running",
            Busy::OtherWindow { .. } => "another Eitri window has this project open; revert from one window at a time",
        }
    }
}

fn running_dir(review_dir: &Path) -> PathBuf {
    review_dir.join("running")
}

fn windows_dir(review_dir: &Path) -> PathBuf {
    review_dir.join("windows")
}

fn running_file(review_dir: &Path, pid: u32, tab: u64) -> PathBuf {
    running_dir(review_dir).join(format!("{pid}.{tab}"))
}

fn io_error(kind: std::io::ErrorKind, what: &str) -> ShadowError {
    ShadowError::Io(std::io::Error::new(kind, what))
}

/// Takes `path`'s exclusive lock, creating the file, and keeps it only when the path still names
/// the locked file: a sweeper that unlinked it between the open and the lock leaves a lock nobody
/// can find, so the file is created and locked again.
fn lock_naming(path: &Path) -> Result<ShadowLock, ShadowError> {
    for _ in 0..LOCK_ATTEMPTS {
        let lock = flock_file(path, libc::LOCK_EX, Some(Instant::now() + LOCK_WAIT))?;
        if lock.names(path) {
            return Ok(lock);
        }
    }
    Err(io_error(
        std::io::ErrorKind::Other,
        "the presence file kept being replaced while it was locked",
    ))
}

/// A window's locks: one for the window, one per running turn.
pub struct Presence {
    review_dir: PathBuf,
    pid: u32,
    window: ShadowLock,
    turns: BTreeMap<u64, ShadowLock>,
}

impl Presence {
    /// Creates `running/` and `windows/` (0700) and takes the window's lock on `windows/<pid>`.
    /// A directory that is a symlink, or not this user's, is refused before anything is created
    /// in it.
    pub fn open(review_dir: &Path, pid: u32) -> Result<Presence, ShadowError> {
        for dir in [running_dir(review_dir), windows_dir(review_dir)] {
            agent::private_fs::create_private_dir_all(&dir, private_root(review_dir))?;
            if !agent::private_fs::is_own_real_dir(&dir) {
                return Err(io_error(
                    std::io::ErrorKind::PermissionDenied,
                    &format!("{} is not a directory of this user's own", dir.display()),
                ));
            }
        }
        let window = lock_naming(&windows_dir(review_dir).join(pid.to_string()))?;
        Ok(Presence {
            review_dir: review_dir.to_path_buf(),
            pid,
            window,
            turns: BTreeMap::new(),
        })
    }

    fn window_path(&self) -> PathBuf {
        windows_dir(&self.review_dir).join(self.pid.to_string())
    }

    /// Marks tab `tab`'s turn as running. A tab id, not a session id: a legacy tab may have no
    /// provider session yet, and the check needs only the window.
    pub fn turn_started(&mut self, tab: u64) -> Result<(), ShadowError> {
        if self.turns.contains_key(&tab) {
            return Ok(());
        }
        let lock = lock_naming(&running_file(&self.review_dir, self.pid, tab))?;
        self.turns.insert(tab, lock);
        Ok(())
    }

    /// Marks tab `tab`'s turn as over: the file is removed while the lock is still held, then the
    /// lock is dropped.
    pub fn turn_ended(&mut self, tab: u64) {
        if self.turns.remove(&tab).is_some() {
            let _ = std::fs::remove_file(running_file(&self.review_dir, self.pid, tab));
        }
    }

    /// Takes the locks again on any file whose path no longer names the file locked, which is what
    /// removing a "stale" file leaves behind when the sweeper met it between its creation and its
    /// locking. Every error is reported after every file has been tried.
    pub fn refresh(&mut self) -> Result<(), ShadowError> {
        let mut first_error = None;
        let window_path = self.window_path();
        if !self.window.names(&window_path) {
            match lock_naming(&window_path) {
                Ok(lock) => self.window = lock,
                Err(e) => first_error = Some(e),
            }
        }
        for (tab, lock) in &mut self.turns {
            let path = running_file(&self.review_dir, self.pid, *tab);
            if !lock.names(&path) {
                match lock_naming(&path) {
                    Ok(fresh) => *lock = fresh,
                    Err(e) => first_error = first_error.or(Some(e)),
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for Presence {
    /// A clean exit removes its files while it still holds them; a killed process leaves them for
    /// the next probe to sweep.
    fn drop(&mut self) {
        for tab in std::mem::take(&mut self.turns).into_keys() {
            let _ = std::fs::remove_file(running_file(&self.review_dir, self.pid, tab));
        }
        let _ = std::fs::remove_file(self.window_path());
    }
}

/// The pid a file under `running/` or `windows/` belongs to: the name up to its first `.`.
fn owner_pid(name: &str) -> Option<u32> {
    name.split('.').next()?.parse().ok()
}

/// The first of `dir`'s files, other than `own_pid`'s, whose lock is held: its pid. Files whose
/// lock is free are leftovers of a process that died, and are removed while the probe's own lock
/// on them is still held, so nobody can take the file between the probe and the removal. A held
/// file of a pid in `skip` (pid, start time) is not reported while that pid is still the process
/// that started at that time.
fn first_held(dir: &Path, own_pid: u32, skip: &[(u32, u64)]) -> Result<Option<u32>, ShadowError> {
    match std::fs::symlink_metadata(dir) {
        // Nobody has made the directory: nobody has anything open.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    // Checked before the listing, which would read through a symlink.
    if !agent::private_fs::is_own_real_dir(dir) {
        return Err(io_error(
            std::io::ErrorKind::PermissionDenied,
            &format!("{} is not a directory of this user's own", dir.display()),
        ));
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut held = None;
    for entry in entries {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(owner_pid) else {
            continue;
        };
        // One's own file is held by this process: opening it again for the probe would conflict
        // with the hold.
        if pid == own_pid {
            continue;
        }
        let path = entry.path();
        match try_flock_existing(&path) {
            Ok(Some(probe)) => {
                // Free, so a leftover. Only the file that was probed is removed: one made again at
                // the same path since is its holder's.
                if probe.names(&path) {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Ok(None) if is_partner(pid, skip) => {}
            Ok(None) => held = held.or(Some(pid)),
            // Gone since the listing, a symlink or not a regular file: not a lock.
            Err(ShadowError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(held)
}

/// Whether `pid` is a linked partner: its pid, and the process that has that pid now is the one
/// that started when the partner did (a pid alone may have been reused since the partner died).
fn is_partner(pid: u32, skip: &[(u32, u64)]) -> bool {
    skip.iter()
        .any(|&(partner_pid, start)| partner_pid == pid && crate::split::proc_start_time(pid) == Some(start))
}

/// What another window is doing that a revert must wait for, and sweeps the leftovers of windows
/// that died. A running turn is reported before an open window.
pub fn busy_elsewhere(review_dir: &Path, own_pid: u32) -> Result<Option<Busy>, ShadowError> {
    busy_elsewhere_beside(review_dir, own_pid, &[])
}

/// [`busy_elsewhere`] for a window linked to the partners in `skip` (pid, start time): their
/// `windows/` files are not other windows. Their `running/` files are checked as anyone's.
fn busy_elsewhere_beside(review_dir: &Path, own_pid: u32, skip: &[(u32, u64)]) -> Result<Option<Busy>, ShadowError> {
    if let Some(pid) = first_held(&running_dir(review_dir), own_pid, &[])? {
        return Ok(Some(Busy::TurnRunning { pid }));
    }
    Ok(first_held(&windows_dir(review_dir), own_pid, skip)?.map(|pid| Busy::OtherWindow { pid }))
}

/// A window this one is linked to, as the check knows it.
#[derive(Debug, Clone)]
pub struct Partner {
    pub pid: u32,
    /// When the partner process started (field 22 of `/proc/<pid>/stat`), so a reused pid is not
    /// taken for it.
    pub start: u64,
    /// Set while the partner has said it could not mark a turn of its own as running: it then
    /// counts as another window. Shared with the link's reader, which sets it before it answers,
    /// so a revert's worker sees it at its next check.
    pub counts_as_other: Arc<AtomicBool>,
}

/// Why a window may not revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    Busy(Busy),
    /// A turn of this window runs.
    TurnRunningHere,
    /// This window's own lock is not held, so it cannot tell others it is here.
    NotHeld(String),
    /// Window 1 refused a module window's write, for the reason it gave.
    Group(String),
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocked::Busy(busy) => f.write_str(busy.reason()),
            Blocked::TurnRunningHere => f.write_str("an agent turn is running"),
            Blocked::NotHeld(why) => write!(f, "this window's presence lock is not held: {why}"),
            Blocked::Group(why) => f.write_str(why),
        }
    }
}

/// Whether the holder's thread has taken the window's lock, or, as the answer to
/// [`PresenceHolder::sync_confirmed`], every file of the set it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// The thread has not finished opening, or is parked.
    NotYet,
    Yes,
    No(String),
}

/// How a module window's write reaches window 1 from its worker thread, off the GTK thread.
pub trait GroupGuard: Send + Sync {
    /// Asks whether the group lets this window write now. Never waits: gives the ask's job, and the
    /// answer comes on the receiver, `Err` saying what blocks it, and a receiver that closes means the
    /// link ended. `None`: there is no link to ask on.
    fn ask(&self) -> Option<(u64, mpsc::Receiver<Result<(), String>>)>;

    /// The worker that asked `job` has stopped waiting: nobody takes its answer any more.
    fn gave_up(&self, job: u64);
}

/// Sent to the presence thread.
enum Message {
    /// The tabs whose turns run now, and where to say once their files are held, if anyone waits for it.
    Running(BTreeSet<u64>, Option<mpsc::Sender<Held>>),
}

/// The window's presence: a thread (`eitri-review-presence`) that owns the [`Presence`], opens it
/// and applies the running set it is sent. Not part of the turn review: every window holds one,
/// whatever its backend and whether or not review is on, because any window's turn or mere
/// presence blocks another window's revert.
pub struct PresenceHolder {
    review_dir: PathBuf,
    pid: u32,
    sender: mpsc::Sender<Message>,
    running_here: Arc<AtomicUsize>,
    held: Arc<Mutex<Held>>,
    last_sent: Mutex<Option<BTreeSet<u64>>>,
    partners: Arc<Mutex<BTreeMap<u32, Partner>>>,
    group: Arc<Mutex<Option<Arc<dyn GroupGuard>>>>,
}

fn set_held(held: &Mutex<Held>, to: Held) {
    *held.lock().unwrap_or_else(|e| e.into_inner()) = to;
}

impl PresenceHolder {
    /// Starts the thread. Never touches the disk itself: the thread opens the files.
    pub fn start(review_dir: PathBuf, pid: u32) -> PresenceHolder {
        Self::start_gated(review_dir, pid, None)
    }

    /// [`start`](Self::start) whose thread waits for `gate` before it opens anything, so a test can
    /// show that nothing on the caller's side waits for the thread, and a prompt stays held for it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn start_parked(review_dir: PathBuf, pid: u32) -> (PresenceHolder, mpsc::Sender<()>) {
        let (release, gate) = mpsc::channel();
        (Self::start_gated(review_dir, pid, Some(gate)), release)
    }

    fn start_gated(review_dir: PathBuf, pid: u32, gate: Option<mpsc::Receiver<()>>) -> PresenceHolder {
        let (sender, messages) = mpsc::channel();
        let held = Arc::new(Mutex::new(Held::NotYet));
        let thread_dir = review_dir.clone();
        let thread_held = Arc::clone(&held);
        let spawned = std::thread::Builder::new()
            .name("eitri-review-presence".into())
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                hold(&thread_dir, pid, &messages, &thread_held);
            });
        if let Err(e) = spawned {
            let why = format!("the presence thread could not start: {e}");
            eprintln!("[review] {why}");
            set_held(&held, Held::No(why));
        }
        PresenceHolder {
            review_dir,
            pid,
            sender,
            running_here: Arc::new(AtomicUsize::new(0)),
            held,
            last_sent: Mutex::new(None),
            partners: Arc::new(Mutex::new(BTreeMap::new())),
            group: Arc::new(Mutex::new(None)),
        }
    }

    /// Tells the thread which tabs run a turn. Memory only: the count the guard reads changes at
    /// once, and the thread takes or drops the files when it gets to it.
    pub fn sync(&self, running: &BTreeSet<u64>) {
        self.running_here.store(running.len(), Ordering::SeqCst);
        let mut last = self.last_sent.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_ref() != Some(running) {
            *last = Some(running.clone());
            // The thread has already ended when this fails; its reason is in `held`.
            let _ = self.sender.send(Message::Running(running.clone(), None));
        }
    }

    /// [`sync`](Self::sync), and an answer once the thread has applied this very set: `Yes` when it
    /// holds the file of every tab in it (and the window's own), `No(why)` when it could not. Sent
    /// even when the set is the one already sent, since a prompt waits for the answer. Never waits:
    /// the caller polls the receiver.
    pub fn sync_confirmed(&self, running: &BTreeSet<u64>) -> mpsc::Receiver<Held> {
        self.running_here.store(running.len(), Ordering::SeqCst);
        let (answer, answered) = mpsc::channel();
        *self.last_sent.lock().unwrap_or_else(|e| e.into_inner()) = Some(running.clone());
        if let Err(mpsc::SendError(Message::Running(_, Some(answer)))) =
            self.sender.send(Message::Running(running.clone(), Some(answer)))
        {
            let why = match self.held.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                Held::No(why) => why,
                Held::NotYet | Held::Yes => "the presence thread is not running".to_string(),
            };
            let _ = answer.send(Held::No(why));
        }
        answered
    }

    pub fn guard(&self) -> PresenceGuard {
        PresenceGuard {
            review_dir: self.review_dir.clone(),
            own_pid: self.pid,
            running_here: Arc::clone(&self.running_here),
            held: Arc::clone(&self.held),
            partners: Arc::clone(&self.partners),
            group: Arc::clone(&self.group),
        }
    }
}

/// The thread's body: open, then keep the files matching the running set and still named by their
/// paths until the holder is dropped.
fn hold(review_dir: &Path, pid: u32, messages: &mpsc::Receiver<Message>, held: &Mutex<Held>) {
    let mut presence = match Presence::open(review_dir, pid) {
        Ok(presence) => presence,
        Err(e) => {
            let why = e.to_string();
            eprintln!("[review] this window's presence could not be held, so it cannot revert: {why}");
            set_held(held, Held::No(why.clone()));
            // Kept alive to answer: a prompt waiting for its file must hear why it never comes,
            // not find the channel closed.
            for Message::Running(_, answer) in messages.iter() {
                if let Some(answer) = answer {
                    let _ = answer.send(Held::No(why.clone()));
                }
            }
            return;
        }
    };
    set_held(held, Held::Yes);
    let mut wanted = BTreeSet::new();
    let mut logged = false;
    loop {
        let mut confirm = None;
        let timed_out = match messages.recv_timeout(REFRESH_EVERY) {
            Ok(Message::Running(set, answer)) => {
                wanted = set;
                confirm = answer;
                false
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut failure = None;
        if timed_out {
            if let Err(e) = presence.refresh() {
                failure = Some(e);
            }
        }
        let current: BTreeSet<u64> = presence.turns.keys().copied().collect();
        for gone in current.difference(&wanted) {
            presence.turn_ended(*gone);
        }
        for new in wanted.difference(&current) {
            if let Err(e) = presence.turn_started(*new) {
                failure = failure.or(Some(e));
            }
        }
        if let Some(confirm) = confirm {
            let all_held = presence.turns.keys().eq(wanted.iter());
            let answer = match &failure {
                Some(e) => Held::No(e.to_string()),
                None if all_held => Held::Yes,
                None => Held::No("the running set's files are not all held".to_string()),
            };
            let _ = confirm.send(answer);
        }
        match failure {
            // A window that cannot tell others it is here, or that a turn runs, must not revert.
            Some(e) => {
                if !logged {
                    eprintln!("[review] this window's presence could not be kept: {e}");
                    logged = true;
                }
                set_held(held, Held::No(e.to_string()));
            }
            None => {
                logged = false;
                set_held(held, Held::Yes);
            }
        }
    }
}

/// What a revert's worker asks before it writes.
#[derive(Clone)]
pub struct PresenceGuard {
    review_dir: PathBuf,
    own_pid: u32,
    running_here: Arc<AtomicUsize>,
    held: Arc<Mutex<Held>>,
    /// This window's linked partners by pid. Shared by every guard of one window, so a partner set
    /// or cleared reaches a worker's check at once.
    partners: Arc<Mutex<BTreeMap<u32, Partner>>>,
    /// In a module window: how its writes ask window 1. Shared as `partners` is.
    group: Arc<Mutex<Option<Arc<dyn GroupGuard>>>>,
}

impl PresenceGuard {
    fn partners(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Partner>> {
        self.partners.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Names the one window this one is linked to (`None`: not linked), in place of any partner
    /// named before. Every guard of this window, those already handed to workers included, skips
    /// that window's `windows/` file from the next check on; cleared on the link's end and before
    /// this window closes it.
    pub fn set_partner(&self, partner: Option<Partner>) {
        let mut partners = self.partners();
        partners.clear();
        partners.extend(partner.map(|p| (p.pid, p)));
    }

    /// Adds a linked window beside the partners already named, for a window with several links. The
    /// same pid again replaces the entry: a relink brings a new flag.
    pub fn add_partner(&self, partner: Partner) {
        self.partners().insert(partner.pid, partner);
    }

    /// Takes one linked window out of the set: its `windows/` file counts again from the next
    /// check on, and the other partners stay.
    pub fn remove_partner(&self, pid: u32) {
        self.partners().remove(&pid);
    }

    /// Makes this a module window's guard: from now on a write asks `group` instead of reading the
    /// presence directories, for this window's guards already handed to workers too.
    pub fn set_group(&self, group: Arc<dyn GroupGuard>) {
        *self.group.lock().unwrap_or_else(|e| e.into_inner()) = Some(group);
    }

    /// How this window's writes ask window 1; `None` in any window but a module window.
    pub(crate) fn group(&self) -> Option<Arc<dyn GroupGuard>> {
        self.group.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The lowest-pid partner that has asked to be counted as another window: it is one whether or not its own
    /// file is there, since a window that cannot mark its turns may not have been able to take that file either.
    fn partner_counted(&self) -> Option<u32> {
        self.partners()
            .values()
            .find(|p| p.counts_as_other.load(Ordering::SeqCst))
            .map(|p| p.pid)
    }

    /// The partners to skip: those that have not asked to be counted as another window.
    fn partners_to_skip(&self) -> Vec<(u32, u64)> {
        self.partners()
            .values()
            .filter(|p| !p.counts_as_other.load(Ordering::SeqCst))
            .map(|p| (p.pid, p.start))
            .collect()
    }

    /// Takes the project's write slot for a revert, an undo or a recovery: the first thing its
    /// worker does, held until after its write.
    pub fn hold_slot_for_write(&self) -> Result<SlotHold, SlotBusy> {
        self.open_slot()?.try_exclusive()
    }

    /// The review directory this window's presence, and its write slot, are kept in.
    pub(crate) fn review_dir(&self) -> &Path {
        &self.review_dir
    }

    fn open_slot(&self) -> Result<WriteSlot, SlotBusy> {
        WriteSlot::open_or_busy(&self.review_dir)
    }

    /// On a worker (it reads the directories and removes leftovers). Refused unless this window's
    /// own lock is held, no turn of this window runs and no other window is busy; a linked partner
    /// that has not asked to be counted is not another window, though its turns are turns. The
    /// answer holds for the moment it is given: the write slot is what keeps writers apart.
    pub fn check(&self) -> Result<(), Blocked> {
        self.check_own()?;
        match busy_elsewhere_beside(&self.review_dir, self.own_pid, &self.partners_to_skip()) {
            Ok(Some(running @ Busy::TurnRunning { .. })) => Err(Blocked::Busy(running)),
            // Read after the listing, so a partner that asks to be counted during it is. It comes before a file
            // the listing found: a counted partner's file is not skipped, and the directory's order is no order.
            Ok(open) => match (self.partner_counted(), open) {
                (Some(pid), _) => Err(Blocked::Busy(Busy::OtherWindow { pid })),
                (None, Some(busy)) => Err(Blocked::Busy(busy)),
                (None, None) => Ok(()),
            },
            // Not knowing is not the same as nobody being there.
            Err(e) => Err(Blocked::NotHeld(format!("the other windows could not be listed: {e}"))),
        }
    }

    /// What this window can say for itself: its own lock is held and no turn of its own runs. Reads
    /// no directory.
    pub(crate) fn check_own(&self) -> Result<(), Blocked> {
        match self.held.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Held::Yes => {}
            Held::NotYet => return Err(Blocked::NotHeld("it has not been taken yet".into())),
            Held::No(why) => return Err(Blocked::NotHeld(why)),
        }
        if self.running_here.load(Ordering::SeqCst) > 0 {
            return Err(Blocked::TurnRunningHere);
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(review_dir: &Path, own_pid: u32, running_here: usize) -> PresenceGuard {
        PresenceGuard {
            review_dir: review_dir.to_path_buf(),
            own_pid,
            running_here: Arc::new(AtomicUsize::new(running_here)),
            held: Arc::new(Mutex::new(Held::Yes)),
            partners: Arc::new(Mutex::new(BTreeMap::new())),
            group: Arc::new(Mutex::new(None)),
        }
    }
}

/// How long a linked window waits for its presence thread to confirm a turn's file, and then for
/// the partner to acknowledge that it counts this window as another window.
pub const CONFIRM_WAIT: Duration = Duration::from_secs(1);

/// What a linked window does next with a prompt waiting on [`PresenceFallback`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing yet: ask again on the next tick.
    Wait,
    /// The turn's file is held, or the partner counts this window as another window, or the link is
    /// gone: the prompt may go out.
    SendPrompt,
    /// Tell the partner `presence{held: false}` and wait for its acknowledgement.
    SendPresenceFalse,
    /// The prompt stays at the head of the tab's queue with this note.
    NotSent(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitStage {
    /// Waiting for the presence thread's answer since the instant held.
    Confirming(Instant),
    /// `presence{held: false}` went out at the instant held; `why` the thread could not confirm.
    AwaitingAck { since: Instant, why: String },
    /// A decision that ends the wait was given.
    Decided,
}

/// What a prompt still waiting for its presence when its window closes the link says, at the head of its queue.
pub const CLOSING_NOTE: &str =
    "not sent: the link to the other window was closed before turn review could mark the turn as running";

/// The wait of one prompt in a linked window, from the presence thread's confirmation through the
/// acknowledged fallback, decided by the clock it is given and nothing else.
///
/// A prompt goes out only once its turn's `running/` file is confirmed held, or once the partner
/// has acknowledged counting this window as another window: in both cases the partner's revert
/// check refuses while the turn may run. With neither within its wait, the prompt is not sent.
#[derive(Debug, Clone)]
pub struct PresenceFallback {
    stage: WaitStage,
}

impl PresenceFallback {
    /// The wait starts as the confirmation is asked for.
    pub fn start(now: Instant) -> PresenceFallback {
        PresenceFallback {
            stage: WaitStage::Confirming(now),
        }
    }

    /// The presence thread answered. A late `Yes` while the acknowledgement is awaited also lets the
    /// prompt go: the file is held either way.
    pub fn on_confirmed(&mut self, held: Held, now: Instant) -> Decision {
        match (&self.stage, held) {
            (WaitStage::Confirming(_) | WaitStage::AwaitingAck { .. }, Held::Yes) => self.decide(Decision::SendPrompt),
            (WaitStage::Confirming(_), Held::No(why)) => self.fall_back(why, now),
            (WaitStage::Confirming(_), Held::NotYet) => self.tick(now),
            (WaitStage::AwaitingAck { .. }, _) => self.tick(now),
            (WaitStage::Decided, _) => Decision::Wait,
        }
    }

    /// The partner acknowledged `presence{held: false}`.
    pub fn on_ack(&mut self, now: Instant) -> Decision {
        match self.tick(now) {
            Decision::Wait if matches!(self.stage, WaitStage::AwaitingAck { .. }) => self.decide(Decision::SendPrompt),
            other => other,
        }
    }

    /// The link ended while the prompt waited. The caller has dropped the partner from the skip
    /// first, so the partner's `windows/` file counts again (or the partner is gone).
    pub fn on_link_end(&mut self) -> Decision {
        match self.stage {
            WaitStage::Decided => Decision::Wait,
            _ => self.decide(Decision::SendPrompt),
        }
    }

    /// This window is closing its link while the prompt waits: the prompt is not sent. The partner may still
    /// be skipping this window's `windows/` file until it hears the link close, and this window's own file has
    /// not been confirmed to mark the turn, so nothing would stop the partner's revert while that turn runs.
    pub fn on_closing(&mut self) -> Decision {
        match self.stage {
            WaitStage::Decided => Decision::Wait,
            _ => self.decide(Decision::NotSent(CLOSING_NOTE.to_string())),
        }
    }

    /// The deadlines: no confirmation within [`CONFIRM_WAIT`] falls back, no acknowledgement within
    /// it after that leaves the prompt unsent.
    pub fn tick(&mut self, now: Instant) -> Decision {
        match &self.stage {
            WaitStage::Confirming(since) if now.saturating_duration_since(*since) >= CONFIRM_WAIT => self.fall_back(
                format!("the presence thread did not answer within {} s", CONFIRM_WAIT.as_secs()),
                now,
            ),
            WaitStage::AwaitingAck { since, why } if now.saturating_duration_since(*since) >= CONFIRM_WAIT => {
                let note = format!("not sent: turn review could not mark the turn as running ({why})");
                self.decide(Decision::NotSent(note))
            }
            _ => Decision::Wait,
        }
    }

    fn fall_back(&mut self, why: String, now: Instant) -> Decision {
        self.stage = WaitStage::AwaitingAck { since: now, why };
        Decision::SendPresenceFalse
    }

    fn decide(&mut self, decision: Decision) -> Decision {
        self.stage = WaitStage::Decided;
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("eitri-presence-unit-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir.join("review")
    }

    #[test]
    fn a_parked_presence_thread_sends_nothing_until_the_partner_acknowledges() {
        let review_dir = scratch("parked");
        let (holder, release) = PresenceHolder::start_parked(review_dir.clone(), std::process::id());
        let start = Instant::now();
        let answer = holder.sync_confirmed(&BTreeSet::from([7]));
        let mut wait = PresenceFallback::start(start);

        // The thread is parked: no answer, and before the deadline nothing goes anywhere.
        assert!(
            answer.try_recv().is_err(),
            "a parked thread cannot have confirmed anything"
        );
        assert_eq!(wait.tick(start + Duration::from_millis(999)), Decision::Wait);

        // Past the deadline the partner is told, and the prompt still waits for its acknowledgement.
        let fell_back = start + CONFIRM_WAIT;
        assert_eq!(wait.tick(fell_back), Decision::SendPresenceFalse);
        assert_eq!(wait.tick(fell_back + Duration::from_millis(500)), Decision::Wait);
        assert!(answer.try_recv().is_err());

        // Only the acknowledgement lets it go.
        assert_eq!(
            wait.on_ack(fell_back + Duration::from_millis(600)),
            Decision::SendPrompt
        );
        assert_eq!(
            wait.tick(fell_back + Duration::from_secs(5)),
            Decision::Wait,
            "decided once"
        );

        // The thread, once it runs, still takes the file and answers.
        release.send(()).unwrap();
        assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(Held::Yes));
        assert!(review_dir.join(format!("running/{}.7", std::process::id())).exists());
        drop(holder);
        let _ = std::fs::remove_dir_all(review_dir.parent().unwrap());
    }

    #[test]
    fn an_ack_after_its_deadline_does_not_send() {
        let start = Instant::now();
        let mut wait = PresenceFallback::start(start);
        assert_eq!(
            wait.on_confirmed(Held::No("disk full".into()), start),
            Decision::SendPresenceFalse
        );
        assert_eq!(
            wait.on_ack(start + CONFIRM_WAIT),
            Decision::NotSent("not sent: turn review could not mark the turn as running (disk full)".into())
        );
    }
}
