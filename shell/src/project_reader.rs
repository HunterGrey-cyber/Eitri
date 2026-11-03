//! Folder reads for the project chooser: one thread per read, registered in a table that judges "stuck" by age, with
//! answers sent to the asking carrier's channel. Only `request` and a `ReadTicket`'s methods run on the GTK thread, and
//! they only take the table's lock, which no thread holds across a filesystem call.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use eitri_core::project_chooser::read::{self, Fs, Mounts, RealFs, MARK_BUDGET, READS_MAX, READ_STUCK};
use eitri_core::project_chooser::{LinkKind, Listing};
use futures_channel::mpsc::UnboundedSender;

/// Where the reader's time comes from.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// The age at which a table entry is stuck, whoever looks.
    pub stuck: Duration,
    /// Reader threads alive at once, stuck ones included.
    pub reads_max: usize,
    /// How long a read may spend on marks and link kinds after its names are sent.
    pub mark_budget: Duration,
}

impl Limits {
    pub(crate) const PROCESS: Limits = Limits {
        stuck: READ_STUCK,
        reads_max: READS_MAX,
        mark_budget: MARK_BUDGET,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadJob {
    pub serial: u64,
    /// The walked folder: answers carry it back unchanged.
    pub dir: PathBuf,
    pub filter: Option<String>,
    /// The name the marks start from.
    pub near: Option<OsString>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadAnswer {
    Listing {
        serial: u64,
        dir: PathBuf,
        listing: Listing,
    },
    Later {
        serial: u64,
        git: Vec<OsString>,
        links: Vec<(OsString, LinkKind)>,
    },
    /// A read that was stuck, or that held another back, has left the table.
    Freed { mount: PathBuf },
}

/// One reader thread, from its request to its end.
pub(crate) struct ReadSlot {
    id: u64,
    started: Instant,
    /// The mount of the call this thread is in, or of its last one while it is between calls.
    in_flight: Option<PathBuf>,
    /// Another read waited for this one's mount, or was refused it.
    held_back: bool,
    /// The last mount this thread entered: what its `Freed` names.
    touched: Option<PathBuf>,
    /// A stuck entry held the mount this read asked for. Its own age then says nothing to anyone: a refused retry
    /// must not start another retry while the stuck thread stays blocked.
    refused: bool,
}

struct Pending {
    job: ReadJob,
    out: UnboundedSender<ReadAnswer>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Table {
    slots: Vec<ReadSlot>,
    /// One job per carrier, waiting for a thread to leave.
    pending: Vec<Pending>,
    #[cfg(test)]
    most: usize,
}

struct Shared {
    table: Mutex<Table>,
    wake: Condvar,
    fs: Arc<dyn Fs>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    next_id: AtomicU64,
    #[cfg(test)]
    finished: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    left: std::sync::atomic::AtomicUsize,
}

/// The process's reader. Tests make their own, so they share nothing.
#[derive(Clone)]
pub(crate) struct Reader {
    shared: Arc<Shared>,
}

/// Says a read is no longer wanted: a job still waiting never starts, and a running one stops between calls.
pub(crate) struct ReadTicket {
    cancel: Arc<AtomicBool>,
    shared: Weak<Shared>,
}

impl ReadTicket {
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        let Some(shared) = self.shared.upgrade() else { return };
        // Under the lock, so a thread about to wait cannot miss this wake-up.
        let mut table = shared.lock();
        table.pending.retain(|p| !Arc::ptr_eq(&p.cancel, &self.cancel));
        shared.wake.notify_all();
    }

    /// A job still waiting for a thread once every thread is stuck is answered `TooManyStuck` now, as a request would
    /// have been: the threads it waits on may never leave. Says whether it was.
    pub(crate) fn too_many_stuck(&self) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        let pending = {
            let mut table = shared.lock();
            let full = table.slots.len() >= shared.limits.reads_max;
            if !full || !table.slots.iter().all(|slot| shared.is_stuck(slot)) {
                return false;
            }
            let Some(at) = table.pending.iter().position(|p| Arc::ptr_eq(&p.cancel, &self.cancel)) else {
                return false;
            };
            table.pending.remove(at)
        };
        let _ = pending.out.unbounded_send(ReadAnswer::Listing {
            serial: pending.job.serial,
            dir: pending.job.dir,
            listing: Listing::TooManyStuck,
        });
        true
    }
}

impl Reader {
    pub(crate) fn global() -> &'static Reader {
        static GLOBAL: OnceLock<Reader> = OnceLock::new();
        GLOBAL.get_or_init(|| Reader::new(Arc::new(RealFs), Arc::new(SystemClock), Limits::PROCESS))
    }

    pub(crate) fn new(fs: Arc<dyn Fs>, clock: Arc<dyn Clock>, limits: Limits) -> Reader {
        Reader {
            shared: Arc::new(Shared {
                table: Mutex::new(Table::default()),
                wake: Condvar::new(),
                fs,
                clock,
                limits,
                next_id: AtomicU64::new(1),
                #[cfg(test)]
                finished: Default::default(),
                #[cfg(test)]
                left: Default::default(),
            }),
        }
    }

    /// Never blocks: a thread starts, the job waits in the carrier's one pending slot, or every thread is stuck and
    /// the answer is `TooManyStuck` at once.
    pub(crate) fn request(&self, job: ReadJob, out: UnboundedSender<ReadAnswer>) -> ReadTicket {
        let shared = &self.shared;
        let cancel = Arc::new(AtomicBool::new(false));
        let ticket = ReadTicket {
            cancel: cancel.clone(),
            shared: Arc::downgrade(shared),
        };
        let entered = {
            let mut table = shared.lock();
            // A carrier has one pending slot: this request replaces whatever it left there.
            table.pending.retain(|p| !p.out.same_receiver(&out));
            if table.slots.len() < shared.limits.reads_max {
                Some(shared.enter(&mut table))
            } else if table.slots.iter().all(|slot| shared.is_stuck(slot)) {
                None
            } else {
                table.pending.push(Pending { job, out, cancel });
                return ticket;
            }
        };
        match entered {
            Some((id, started)) => Shared::spawn(shared, id, started, job, out, cancel),
            None => {
                let _ = out.unbounded_send(ReadAnswer::Listing {
                    serial: job.serial,
                    dir: job.dir,
                    listing: Listing::TooManyStuck,
                });
            }
        }
        ticket
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_stuck(&self, slot: &ReadSlot) -> bool {
        self.clock.now().saturating_duration_since(slot.started) >= self.limits.stuck
    }

    /// Registers a new thread's entry.
    fn enter(&self, table: &mut Table) -> (u64, Instant) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let started = self.clock.now();
        table.slots.push(ReadSlot {
            id,
            started,
            in_flight: None,
            held_back: false,
            touched: None,
            refused: false,
        });
        #[cfg(test)]
        {
            table.most = table.most.max(table.slots.len());
        }
        (id, started)
    }

    fn spawn(
        this: &Arc<Shared>,
        id: u64,
        started: Instant,
        job: ReadJob,
        out: UnboundedSender<ReadAnswer>,
        cancel: Arc<AtomicBool>,
    ) {
        let (serial, dir, refused) = (job.serial, job.dir.clone(), out.clone());
        let shared = this.clone();
        let spawned = std::thread::Builder::new()
            .name("project-read".into())
            .spawn(move || shared.run(id, started, job, out, cancel));
        if spawned.is_err() {
            this.lock().slots.retain(|slot| slot.id != id);
            this.wake.notify_all();
            let _ = refused.unbounded_send(ReadAnswer::Listing {
                serial,
                dir,
                listing: Listing::Stuck,
            });
        }
    }

    /// One read, on its own thread: the names first, then the marks and link kinds.
    fn run(
        self: Arc<Shared>,
        id: u64,
        started: Instant,
        job: ReadJob,
        out: UnboundedSender<ReadAnswer>,
        cancel: Arc<AtomicBool>,
    ) {
        let _leave = Leave {
            shared: self.clone(),
            id,
            out: out.clone(),
        };
        if cancel.load(Ordering::Acquire) {
            return;
        }
        // procfs: reading it enters no mounted filesystem, so no mount is charged for it. Without it every call is
        // charged to `/`, which still lets one thread at a time in.
        let mounts = Mounts::parse(&self.fs.mountinfo().unwrap_or_default());
        let fs = &*self.fs;
        let listing = read::read(fs, &job.dir, job.filter.as_deref(), &mounts, &mut |mount| {
            self.touch(id, mount, &cancel, None)
        });
        self.clear(id);
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let marks = match &listing {
            Listing::Read { dir, entries, .. } if !entries.is_empty() => Some((dir.clone(), entries.clone())),
            _ => None,
        };
        let _ = out.unbounded_send(ReadAnswer::Listing {
            serial: job.serial,
            dir: job.dir,
            listing,
        });
        if let Some((physical, entries)) = marks {
            let begun = self.clock.now();
            let left = self
                .limits
                .stuck
                .saturating_sub(begun.saturating_duration_since(started))
                .min(self.limits.mark_budget);
            let (git, links) = read::later(
                fs,
                &physical,
                &entries,
                job.near.as_deref(),
                &mounts,
                &mut |mount| self.touch(id, mount, &cancel, Some(begun)),
                Instant::now() + left,
            );
            self.clear(id);
            if !cancel.load(Ordering::Acquire) && (!git.is_empty() || !links.is_empty()) {
                let _ = out.unbounded_send(ReadAnswer::Later {
                    serial: job.serial,
                    git,
                    links,
                });
            }
        }
        #[cfg(test)]
        self.finished.fetch_add(1, Ordering::AcqRel);
    }

    /// Runs before every blocking call: whether thread `id` may enter `mount`. It first gives up the mount of its own
    /// last call, so two threads crossing mounts never wait on each other; then it waits while a young entry is in a
    /// call on `mount`, and is refused once that entry is stuck. `later` is when the marks began: they stop at the
    /// thread's stuck age and at the marks' budget.
    fn touch(&self, id: u64, mount: &Path, cancel: &AtomicBool, later: Option<Instant>) -> bool {
        let mut table = self.lock();
        let Some(mine) = table.slots.iter().position(|slot| slot.id == id) else {
            return false;
        };
        table.slots[mine].in_flight = None;
        self.wake.notify_all();
        loop {
            if cancel.load(Ordering::Acquire) {
                return false;
            }
            let now = self.clock.now();
            let Some(mine) = table.slots.iter().position(|slot| slot.id == id) else {
                return false;
            };
            // In the marks, a wait ends at this thread's own stuck age or budget, whichever comes first.
            let mut own_left = Duration::MAX;
            if let Some(begun) = later {
                let age = now.saturating_duration_since(table.slots[mine].started);
                let spent = now.saturating_duration_since(begun);
                if age >= self.limits.stuck || spent >= self.limits.mark_budget {
                    return false;
                }
                own_left = (self.limits.stuck - age).min(self.limits.mark_budget - spent);
            }
            let holder = table
                .slots
                .iter_mut()
                .find(|slot| slot.id != id && slot.in_flight.as_deref() == Some(mount));
            let Some(holder) = holder else {
                let slot = &mut table.slots[mine];
                slot.in_flight = Some(mount.to_path_buf());
                slot.touched = Some(mount.to_path_buf());
                return true;
            };
            holder.held_back = true;
            let age = now.saturating_duration_since(holder.started);
            if age >= self.limits.stuck {
                table.slots[mine].refused = true;
                return false;
            }
            let wait = (self.limits.stuck - age).min(own_left);
            table = match self.wake.wait_timeout(table, wait) {
                Ok((table, _)) => table,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Thread `id` is between calls with nothing ahead of it on its mount.
    fn clear(&self, id: u64) {
        let mut table = self.lock();
        if let Some(slot) = table.slots.iter_mut().find(|slot| slot.id == id) {
            slot.in_flight = None;
        }
        self.wake.notify_all();
    }
}

/// A thread's end, however it ends: only this removes its entry. An entry that was stuck or held another read back
/// says `Freed` to its own carrier; the next pending job takes its place.
struct Leave {
    shared: Arc<Shared>,
    id: u64,
    out: UnboundedSender<ReadAnswer>,
}

impl Drop for Leave {
    fn drop(&mut self) {
        let shared = &self.shared;
        let (freed, next) = {
            let mut table = shared.lock();
            let Some(at) = table.slots.iter().position(|slot| slot.id == self.id) else {
                return;
            };
            let slot = table.slots.remove(at);
            shared.wake.notify_all();
            let freed = (slot.held_back || (shared.is_stuck(&slot) && !slot.refused))
                .then_some(slot.touched)
                .flatten();
            let mut next = None;
            while table.slots.len() < shared.limits.reads_max && !table.pending.is_empty() {
                let pending = table.pending.remove(0);
                if pending.cancel.load(Ordering::Acquire) || pending.out.is_closed() {
                    continue;
                }
                next = Some((shared.enter(&mut table), pending));
                break;
            }
            (freed, next)
        };
        if let Some(mount) = freed {
            let _ = self.out.unbounded_send(ReadAnswer::Freed { mount });
        }
        if let Some(((id, started), pending)) = next {
            Shared::spawn(shared, id, started, pending.job, pending.out, pending.cancel);
        }
        #[cfg(test)]
        shared.left.fetch_add(1, Ordering::AcqRel);
    }
}

pub(crate) const BROWSE_STUCK: &str = "[project] browse stuck";
pub(crate) const BROWSE_LATE: &str = "[project] browse late";
pub(crate) const BROWSE_FREED: &str = "[project] browse freed";

/// A carrier's reads: the newest request supersedes every older one, and only its answers pass.
#[derive(Default)]
pub(crate) struct Desk {
    asked: Option<Asked>,
}

struct Asked {
    serial: u64,
    /// A typed job still waiting out its debounce.
    waiting: Option<ReadJob>,
    ticket: Option<ReadTicket>,
    listed: bool,
    stuck: bool,
}

impl Desk {
    /// Takes a new request and returns the job to start now; a debounced one waits for [`Desk::due`].
    pub(crate) fn ask(&mut self, job: ReadJob, debounce: bool) -> Option<ReadJob> {
        if let Some(ticket) = self.asked.take().and_then(|old| old.ticket) {
            ticket.cancel();
        }
        let serial = job.serial;
        let (waiting, now) = if debounce { (Some(job), None) } else { (None, Some(job)) };
        self.asked = Some(Asked {
            serial,
            waiting,
            ticket: None,
            listed: false,
            stuck: false,
        });
        now
    }

    /// The debounce of `serial` has passed: its job, if it is still the newest and has not started.
    pub(crate) fn due(&mut self, serial: u64) -> Option<ReadJob> {
        self.asked
            .as_mut()
            .filter(|asked| asked.serial == serial)?
            .waiting
            .take()
    }

    pub(crate) fn started(&mut self, serial: u64, ticket: ReadTicket) {
        match self.asked.as_mut().filter(|asked| asked.serial == serial) {
            Some(asked) => asked.ticket = Some(ticket),
            None => ticket.cancel(),
        }
    }

    fn open(&mut self, serial: u64) -> Option<&mut Asked> {
        self.asked
            .as_mut()
            .filter(|asked| asked.serial == serial && asked.waiting.is_none() && !asked.listed)
    }

    /// Whether `serial` is still waiting for its listing when its slow timer comes due.
    pub(crate) fn slow(&self, serial: u64) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|asked| asked.serial == serial && asked.waiting.is_none() && !asked.listed)
    }

    /// The line to log when `serial` is still waiting for its listing at the stuck age; the serial stays open. A job
    /// that never got a thread is answered `TooManyStuck` instead when every thread is stuck, and logs nothing here.
    pub(crate) fn stuck(&mut self, serial: u64) -> Option<&'static str> {
        let asked = self.open(serial)?;
        if asked.ticket.as_ref().is_some_and(ReadTicket::too_many_stuck) {
            return None;
        }
        asked.stuck = true;
        Some(BROWSE_STUCK)
    }

    /// An answer from the reader, with the lines to log, if it is one the chooser still wants.
    pub(crate) fn take(&mut self, answer: ReadAnswer) -> Option<(ReadAnswer, Vec<String>)> {
        let lines = match &answer {
            ReadAnswer::Freed { .. } => vec![BROWSE_FREED.to_owned()],
            ReadAnswer::Listing { serial, listing, .. } => {
                let asked = self.open(*serial)?;
                asked.listed = true;
                let late = asked.stuck.then(|| BROWSE_LATE.to_owned());
                // A refusal read nothing; any other answer is a folder that was read, listable or not.
                let count = match listing {
                    Listing::Stuck | Listing::TooManyStuck => None,
                    Listing::Read { entries, .. } => Some(entries.len()),
                    _ => Some(0),
                };
                late.into_iter()
                    .chain(count.map(|n| format!("[project] browse read {n}")))
                    .collect()
            }
            ReadAnswer::Later { serial, .. } => {
                self.asked
                    .as_ref()
                    .filter(|asked| asked.serial == *serial && asked.listed)?;
                Vec::new()
            }
        };
        Some((answer, lines))
    }
}

#[cfg(test)]
pub(crate) struct SlotView {
    pub stuck: bool,
    pub held_back: bool,
}

#[cfg(test)]
impl Reader {
    fn slots(&self) -> Vec<SlotView> {
        let now = self.shared.clock.now();
        self.shared
            .lock()
            .slots
            .iter()
            .map(|s| SlotView {
                stuck: now.saturating_duration_since(s.started) >= self.shared.limits.stuck,
                held_back: s.held_back,
            })
            .collect()
    }

    fn pending(&self) -> usize {
        self.shared.lock().pending.len()
    }

    fn most(&self) -> usize {
        self.shared.lock().most
    }

    fn finished(&self) -> usize {
        self.shared.finished.load(Ordering::Acquire)
    }

    fn left(&self) -> usize {
        self.shared.left.load(Ordering::Acquire)
    }

    /// Wakes waiting threads after the test clock moved.
    fn poke(&self) {
        let _table = self.shared.lock();
        self.shared.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::project_chooser::read::{DType, Kind};
    use eitri_core::project_chooser::{Entry, EntryKind};
    use futures_channel::mpsc::{unbounded, UnboundedReceiver};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;

    /// A bound for a test that has gone wrong, never a timing claim: every wait below ends on a condition.
    const BROKEN: Duration = Duration::from_secs(20);

    const MOUNTINFO: &str = "\
1 0 8:1 / / rw - ext4 /dev/root rw
2 1 0:40 / /mnt/nas rw - nfs4 server:/export rw
3 1 0:41 / /mnt/nas2 rw - nfs4 server:/other rw
4 1 8:2 / /mnt/disk rw - ext4 /dev/sdb rw
";

    #[derive(Default)]
    struct Calls {
        held: BTreeSet<PathBuf>,
        held_paths: BTreeSet<PathBuf>,
        blocked: Vec<PathBuf>,
        every: Vec<(String, PathBuf)>,
        in_call: BTreeMap<PathBuf, usize>,
        most_in_call: BTreeMap<PathBuf, usize>,
    }

    /// A tree of folders and links whose calls block, by mount or by path, until the test lets them go.
    struct HeldFs {
        dirs: BTreeSet<PathBuf>,
        links: BTreeMap<PathBuf, PathBuf>,
        mounts: Mounts,
        calls: Mutex<Calls>,
        changed: Condvar,
    }

    impl HeldFs {
        fn new(dirs: &[&str]) -> Arc<HeldFs> {
            let mut all = BTreeSet::new();
            for dir in dirs {
                let mut dir = PathBuf::from(dir);
                loop {
                    all.insert(dir.clone());
                    if !dir.pop() {
                        break;
                    }
                }
            }
            Arc::new(HeldFs {
                dirs: all,
                links: BTreeMap::new(),
                mounts: Mounts::parse(MOUNTINFO),
                calls: Mutex::new(Calls::default()),
                changed: Condvar::new(),
            })
        }

        fn calls(&self) -> MutexGuard<'_, Calls> {
            self.calls.lock().unwrap()
        }

        fn hold(&self, mount: &str) {
            self.calls().held.insert(mount.into());
        }

        fn release(&self, mount: &str) {
            self.calls().held.remove(Path::new(mount));
            self.changed.notify_all();
        }

        fn hold_path(&self, path: &str) {
            self.calls().held_paths.insert(path.into());
        }

        fn release_path(&self, path: &str) {
            self.calls().held_paths.remove(Path::new(path));
            self.changed.notify_all();
        }

        /// Waits until `count` calls under `prefix` are blocked.
        fn wait_blocked(&self, prefix: &str, count: usize) {
            let end = Instant::now() + BROKEN;
            let mut calls = self.calls();
            while calls.blocked.iter().filter(|p| p.starts_with(prefix)).count() < count {
                assert!(Instant::now() < end, "no call under {prefix} blocked");
                calls = self.changed.wait_timeout(calls, Duration::from_millis(50)).unwrap().0;
            }
        }

        fn every(&self) -> Vec<(String, PathBuf)> {
            self.calls().every.clone()
        }

        fn enter(&self, p: &Path) {
            let name = std::thread::current().name().unwrap_or_default().to_owned();
            let mount = self.mounts.of(p).to_path_buf();
            let mut calls = self.calls();
            calls.every.push((name, p.to_path_buf()));
            let now = {
                let n = calls.in_call.entry(mount.clone()).or_default();
                *n += 1;
                *n
            };
            let most = calls.most_in_call.entry(mount.clone()).or_default();
            *most = (*most).max(now);
            if calls.held.contains(&mount) || calls.held_paths.contains(p) {
                calls.blocked.push(p.to_path_buf());
                self.changed.notify_all();
                while calls.held.contains(&mount) || calls.held_paths.contains(p) {
                    calls = self.changed.wait(calls).unwrap();
                }
                let at = calls.blocked.iter().position(|b| b == p).unwrap();
                calls.blocked.remove(at);
            }
        }

        fn leave(&self, p: &Path) {
            let mount = self.mounts.of(p).to_path_buf();
            *self.calls().in_call.get_mut(&mount).unwrap() -= 1;
            self.changed.notify_all();
        }
    }

    impl Fs for HeldFs {
        fn lstat(&self, p: &Path) -> io::Result<Kind> {
            self.enter(p);
            let answer = if self.links.contains_key(p) {
                Ok(Kind::Link)
            } else if self.dirs.contains(p) {
                Ok(Kind::Dir)
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            };
            self.leave(p);
            answer
        }

        fn readlink(&self, p: &Path) -> io::Result<PathBuf> {
            self.enter(p);
            let answer = self
                .links
                .get(p)
                .cloned()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL));
            self.leave(p);
            answer
        }

        fn read_dir(&self, p: &Path, each: &mut dyn FnMut(OsString, DType) -> bool) -> io::Result<()> {
            self.enter(p);
            let children: Vec<_> = self
                .dirs
                .iter()
                .filter(|d| d.parent() == Some(p))
                .map(|d| (d.file_name().unwrap().to_owned(), DType::Dir))
                .chain(
                    self.links
                        .keys()
                        .filter(|d| d.parent() == Some(p))
                        .map(|d| (d.file_name().unwrap().to_owned(), DType::Link)),
                )
                .collect();
            self.leave(p);
            for (name, kind) in children {
                if !each(name, kind) {
                    break;
                }
            }
            Ok(())
        }

        fn mountinfo(&self) -> io::Result<String> {
            Ok(MOUNTINFO.into())
        }
    }

    struct TestClock(Mutex<Instant>);

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    const LIMITS: Limits = Limits {
        stuck: Duration::from_secs(2),
        reads_max: 3,
        mark_budget: Duration::from_secs(600),
    };

    fn reader(fs: &Arc<HeldFs>) -> (Reader, Arc<TestClock>) {
        let clock = Arc::new(TestClock(Mutex::new(Instant::now())));
        (Reader::new(fs.clone(), clock.clone(), LIMITS), clock)
    }

    fn advance(reader: &Reader, clock: &TestClock, by: Duration) {
        *clock.0.lock().unwrap() += by;
        reader.poke();
    }

    fn job(serial: u64, dir: &str) -> ReadJob {
        ReadJob {
            serial,
            dir: dir.into(),
            filter: None,
            near: None,
        }
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let end = Instant::now() + BROKEN;
        while !done() {
            assert!(Instant::now() < end, "never: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn next(rx: &mut UnboundedReceiver<ReadAnswer>) -> ReadAnswer {
        let end = Instant::now() + BROKEN;
        loop {
            if let Ok(answer) = rx.try_recv() {
                return answer;
            }
            assert!(Instant::now() < end, "no answer came");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// What has come so far; call it once the threads that could send are known to have left.
    fn drain(rx: &mut UnboundedReceiver<ReadAnswer>) -> Vec<ReadAnswer> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn freed(answers: &[ReadAnswer]) -> Vec<&Path> {
        answers
            .iter()
            .filter_map(|a| match a {
                ReadAnswer::Freed { mount } => Some(mount.as_path()),
                _ => None,
            })
            .collect()
    }

    fn listing_of(answer: &ReadAnswer) -> (u64, &Listing) {
        match answer {
            ReadAnswer::Listing { serial, listing, .. } => (*serial, listing),
            other => panic!("not a listing: {other:?}"),
        }
    }

    fn names(listing: &Listing) -> Vec<String> {
        match listing {
            Listing::Read { entries, .. } => entries.iter().map(|e| e.name.clone()).collect(),
            other => panic!("not read: {other:?}"),
        }
    }

    #[test]
    fn stuck_is_age_whoever_looks() {
        let fs = HeldFs::new(&["/mnt/nas/a", "/mnt/nas/b"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, mut rx) = unbounded();
        let _first = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        advance(&reader, &clock, LIMITS.stuck - Duration::from_millis(1));
        assert!(!reader.slots()[0].stuck, "younger than the stuck age");
        advance(&reader, &clock, Duration::from_millis(1));
        assert!(reader.slots()[0].stuck, "no timer and no chooser had to say so");
        // A second chooser meets the entry by its age alone and is refused without a call.
        let (tx2, mut rx2) = unbounded();
        let _second = reader.request(job(2, "/mnt/nas/b"), tx2);
        assert_eq!(listing_of(&next(&mut rx2)), (2, &Listing::Stuck));
        fs.release("/mnt/nas");
        wait_until("both threads left", || reader.left() == 2);
        let answers = drain(&mut rx);
        assert_eq!(listing_of(&answers[0]).0, 1, "the late listing still comes");
        assert_eq!(freed(&answers), [Path::new("/mnt/nas")]);
        assert!(drain(&mut rx2).is_empty());
    }

    #[test]
    fn a_mount_holds_one_thread_in_a_call() {
        let fs = HeldFs::new(&["/mnt/nas/a", "/mnt/nas/b", "/mnt/nas/c"]);
        let (reader, _clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let mut rxs = Vec::new();
        let mut tickets = Vec::new();
        for (serial, dir) in [(1, "/mnt/nas/a"), (2, "/mnt/nas/b"), (3, "/mnt/nas/c")] {
            let (tx, rx) = unbounded();
            tickets.push(reader.request(job(serial, dir), tx));
            rxs.push(rx);
        }
        fs.wait_blocked("/mnt/nas", 1);
        wait_until("the other two reach the mount", || {
            fs.every().iter().filter(|(_, p)| p == Path::new("/mnt")).count() == 3
                && reader.slots().iter().any(|s| s.held_back)
        });
        assert_eq!(fs.calls().in_call[Path::new("/mnt/nas")], 1);
        fs.release("/mnt/nas");
        for (rx, name) in rxs.iter_mut().zip(["a", "b", "c"]) {
            let answer = next(rx);
            assert!(
                matches!(listing_of(&answer).1, Listing::Read { .. }),
                "{name}: {answer:?}"
            );
        }
        wait_until("all left", || reader.left() == 3);
        assert_eq!(
            fs.calls().most_in_call[Path::new("/mnt/nas")],
            1,
            "one thread in a call per mount"
        );
    }

    #[test]
    fn a_closed_choosers_thread_still_counts() {
        let fs = HeldFs::new(&["/mnt/nas/a", "/mnt/nas/b"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, rx) = unbounded();
        let _closed = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        drop(rx);
        // The next opening asks within the stuck age: it waits for the old thread, then is refused at that age.
        let (tx2, mut rx2) = unbounded();
        let _reopened = reader.request(job(2, "/mnt/nas/b"), tx2);
        wait_until("the new read waits on the old thread", || reader.slots()[0].held_back);
        assert!(drain(&mut rx2).is_empty(), "a young holder is waited for");
        assert_eq!(fs.calls().in_call[Path::new("/mnt/nas")], 1);
        advance(&reader, &clock, LIMITS.stuck);
        assert_eq!(listing_of(&next(&mut rx2)), (2, &Listing::Stuck));
        wait_until("the refused thread left", || reader.left() == 1);
        assert_eq!(
            reader.slots().len(),
            1,
            "the closed chooser's thread is still in the table"
        );
        fs.release("/mnt/nas");
        wait_until("the old thread left", || reader.left() == 2);
        assert!(
            drain(&mut rx2).is_empty(),
            "the refused read says nothing, though it waited past the stuck age; the old thread's Freed goes to its \
             own channel, which is closed"
        );
    }

    /// Three reads blocked in three mounts.
    fn three_blocked(fs: &Arc<HeldFs>, reader: &Reader) -> Vec<(ReadTicket, UnboundedReceiver<ReadAnswer>)> {
        let mut held = Vec::new();
        for (serial, mount) in [(1, "/mnt/nas"), (2, "/mnt/nas2"), (3, "/mnt/disk")] {
            fs.hold(mount);
            let (tx, rx) = unbounded();
            held.push((reader.request(job(serial, &format!("{mount}/a")), tx), rx));
            fs.wait_blocked(mount, 1);
        }
        held
    }

    fn three_fs() -> Arc<HeldFs> {
        HeldFs::new(&["/mnt/nas/a", "/mnt/nas2/a", "/mnt/disk/a", "/srv/x/one", "/srv/y/two"])
    }

    #[test]
    fn request_returns_while_three_reads_are_blocked() {
        let fs = three_fs();
        let (reader, _clock) = reader(&fs);
        let _held = three_blocked(&fs, &reader);
        let (tx, _rx) = unbounded();
        let (done, returned) = std::sync::mpsc::channel();
        let asker = reader.clone();
        std::thread::spawn(move || {
            let ticket = asker.request(job(4, "/srv/x"), tx);
            let _ = done.send(ticket);
        });
        let _ticket = returned
            .recv_timeout(BROKEN)
            .expect("request returned with three reads blocked");
        assert_eq!(reader.slots().len(), 3);
        assert_eq!(reader.pending(), 1);
        for mount in ["/mnt/nas", "/mnt/nas2", "/mnt/disk"] {
            fs.release(mount);
        }
        wait_until("all left", || reader.left() == 4);
    }

    #[test]
    fn the_pending_job_starts_when_a_thread_leaves() {
        let fs = three_fs();
        let (reader, _clock) = reader(&fs);
        let mut held = three_blocked(&fs, &reader);
        let (tx, mut rx) = unbounded();
        let _older = reader.request(job(4, "/srv/x"), tx.clone());
        // The same carrier asks again: the newer job takes the one pending slot.
        let _newer = reader.request(job(5, "/srv/y"), tx);
        assert_eq!(reader.pending(), 1);
        fs.release("/mnt/nas2");
        assert!(matches!(listing_of(&next(&mut held[1].1)).1, Listing::Read { .. }));
        let answer = next(&mut rx);
        assert_eq!(listing_of(&answer).0, 5);
        assert_eq!(names(listing_of(&answer).1), ["two"]);
        wait_until("the pending job's thread left", || reader.left() == 2);
        assert!(drain(&mut rx).is_empty(), "the replaced job never ran");
        assert_eq!(reader.most(), 3, "never more than three threads");
        fs.release("/mnt/nas");
        fs.release("/mnt/disk");
        wait_until("all left", || reader.left() == 4);
    }

    #[test]
    fn three_stuck_answers_too_many_at_once() {
        let fs = three_fs();
        let (reader, clock) = reader(&fs);
        let _held = three_blocked(&fs, &reader);
        advance(&reader, &clock, LIMITS.stuck);
        let (tx, mut rx) = unbounded();
        let _ticket = reader.request(job(4, "/srv/x"), tx);
        assert_eq!(
            rx.try_recv().ok().as_ref().map(listing_of),
            Some((4, &Listing::TooManyStuck)),
            "answered before request returned"
        );
        assert_eq!((reader.slots().len(), reader.pending()), (3, 0));
        for mount in ["/mnt/nas", "/mnt/nas2", "/mnt/disk"] {
            fs.release(mount);
        }
        wait_until("all left", || reader.left() == 3);
    }

    #[test]
    fn a_cancelled_read_stops_between_calls() {
        let fs = HeldFs::new(&["/mnt/disk/a/x"]);
        let (reader, _clock) = reader(&fs);
        fs.hold_path("/mnt/disk/a");
        let (tx, mut rx) = unbounded();
        let ticket = reader.request(job(1, "/mnt/disk/a"), tx);
        fs.wait_blocked("/mnt/disk/a", 1);
        ticket.cancel();
        let calls = fs.every().len();
        fs.release_path("/mnt/disk/a");
        wait_until("the thread left", || reader.left() == 1);
        assert_eq!(fs.every().len(), calls, "no call after the one it was in");
        assert!(drain(&mut rx).is_empty(), "a cancelled read answers nothing");
    }

    #[test]
    fn rapid_requests_supersede_and_never_exceed_three_threads() {
        let dirs: Vec<String> = (0..10).map(|n| format!("/mnt/nas/d{n}/inner{n}")).collect();
        let fs = HeldFs::new(&dirs.iter().map(String::as_str).collect::<Vec<_>>());
        let (reader, _clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let mut desk = Desk::default();
        let (tx, mut rx) = unbounded();
        for serial in 1..=10u64 {
            let job = desk
                .ask(job(serial, &format!("/mnt/nas/d{}", serial - 1)), false)
                .unwrap();
            let ticket = reader.request(job, tx.clone());
            desk.started(serial, ticket);
            assert!(reader.slots().len() <= 3);
        }
        fs.release("/mnt/nas");
        wait_until("every thread left", || {
            reader.slots().is_empty() && reader.pending() == 0
        });
        assert!(reader.most() <= 3, "{} threads", reader.most());
        let mut passed = Vec::new();
        for answer in drain(&mut rx) {
            if let Some((ReadAnswer::Listing { serial, listing, .. }, _)) = desk.take(answer) {
                passed.push((serial, names(&listing)));
            }
        }
        assert_eq!(
            passed,
            [(10, vec!["inner9".to_owned()])],
            "the folder shown is the last asked for"
        );
    }

    #[test]
    fn a_stuck_mount_does_not_hold_reads_on_another_mount() {
        let fs = HeldFs::new(&["/mnt/nas/a", "/mnt/nas2/a/x", "/mnt/disk/a/y"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, _rx) = unbounded();
        let _stuck = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        advance(&reader, &clock, LIMITS.stuck);
        // `/mnt/nas2` is a mount of its own, not a folder under `/mnt/nas`.
        for (serial, dir, name) in [(2, "/mnt/nas2/a", "x"), (3, "/mnt/disk/a", "y")] {
            let (tx, mut rx) = unbounded();
            let _ticket = reader.request(job(serial, dir), tx);
            let answer = next(&mut rx);
            assert_eq!(listing_of(&answer).0, serial);
            assert_eq!(names(listing_of(&answer).1), [name]);
        }
        fs.release("/mnt/nas");
        wait_until("all left", || reader.left() == 3);
    }

    #[test]
    fn touch_clears_its_own_in_flight_before_waiting() {
        let fs = HeldFs::new(&[]);
        let (reader, _clock) = reader(&fs);
        let (a, b) = {
            let mut table = reader.shared.lock();
            (reader.shared.enter(&mut table).0, reader.shared.enter(&mut table).0)
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (said, heard) = std::sync::mpsc::channel();
        let mut goes = Vec::new();
        for (id, first, second) in [(a, "/m1", "/m2"), (b, "/m2", "/m1")] {
            let (go, wait) = std::sync::mpsc::channel::<()>();
            goes.push(go);
            let (shared, cancel, said) = (reader.shared.clone(), cancel.clone(), said.clone());
            std::thread::spawn(move || {
                let entered = shared.touch(id, Path::new(first), &cancel, None);
                said.send((id, first, entered)).unwrap();
                wait.recv().unwrap();
                let crossed = shared.touch(id, Path::new(second), &cancel, None);
                said.send((id, second, crossed)).unwrap();
            });
        }
        for _ in 0..2 {
            assert!(heard.recv_timeout(BROKEN).unwrap().2);
        }
        // Each now holds the mount of its last call and asks for the other's.
        for go in goes {
            go.send(()).unwrap();
        }
        for _ in 0..2 {
            let (id, mount, entered) = heard.recv_timeout(BROKEN).expect("neither waits on the other for ever");
            assert!(entered, "{id} refused {mount}");
        }
        assert!(reader.slots().iter().all(|s| !s.stuck));
    }

    /// Real time, moved ahead by `ahead`: a wait on the condvar then runs in the same time this clock reads.
    struct Ahead(Mutex<Duration>);

    impl Clock for Ahead {
        fn now(&self) -> Instant {
            Instant::now() + *self.0.lock().unwrap()
        }
    }

    /// A young holder in a long call on the mount, and a mark that asks for it: the mark gives up at its own budget or
    /// its own stuck age, not at the holder's.
    #[test]
    fn a_mark_waits_no_longer_than_its_own_budget_or_age() {
        for (budget, aged) in [(Duration::from_millis(50), false), (Duration::from_secs(3000), true)] {
            let fs = HeldFs::new(&[]);
            let clock = Arc::new(Ahead(Mutex::new(Duration::ZERO)));
            let limits = Limits {
                stuck: Duration::from_secs(3600),
                reads_max: 3,
                mark_budget: budget,
            };
            let reader = Reader::new(fs.clone(), clock.clone(), limits);
            let (holder, marker) = {
                let mut table = reader.shared.lock();
                (reader.shared.enter(&mut table).0, reader.shared.enter(&mut table).0)
            };
            let cancel = AtomicBool::new(false);
            assert!(reader.shared.touch(holder, Path::new("/m"), &cancel, None));
            if aged {
                // The marker is a breath short of its stuck age; the holder is made young again.
                *clock.0.lock().unwrap() = limits.stuck - Duration::from_millis(50);
                let now = clock.now();
                reader.shared.lock().slots[0].started = now;
            }
            let begun = clock.now();
            let (said, heard) = std::sync::mpsc::channel();
            let shared = reader.shared.clone();
            std::thread::spawn(move || {
                let cancel = AtomicBool::new(false);
                let _ = said.send(shared.touch(marker, Path::new("/m"), &cancel, Some(begun)));
            });
            let entered = heard
                .recv_timeout(BROKEN)
                .unwrap_or_else(|_| panic!("aged {aged}: the mark waited on the holder's age"));
            assert!(!entered, "aged {aged}: the holder is still in its call");
            assert!(reader.slots()[0].held_back);
            assert!(!reader.shared.lock().slots[1].refused, "giving up is not a refusal");
        }
    }

    #[test]
    fn a_waiting_job_is_answered_too_many_once_every_thread_is_stuck() {
        let fs = three_fs();
        let (reader, clock) = reader(&fs);
        let _held = three_blocked(&fs, &reader);
        let mut desk = Desk::default();
        let (tx, mut rx) = unbounded();
        let job = desk.ask(job(4, "/srv/x"), false).unwrap();
        desk.started(4, reader.request(job, tx));
        assert_eq!(reader.pending(), 1, "three young threads: the job waits");
        advance(&reader, &clock, LIMITS.stuck);
        assert_eq!(desk.stuck(4), None, "no stuck line for a read that never started");
        let (answer, lines) = desk.take(next(&mut rx)).expect("the refusal passes");
        assert_eq!(listing_of(&answer), (4, &Listing::TooManyStuck));
        assert!(lines.is_empty(), "a refusal logs nothing: {lines:?}");
        assert_eq!(reader.pending(), 0);
        for mount in ["/mnt/nas", "/mnt/nas2", "/mnt/disk"] {
            fs.release(mount);
        }
        wait_until("all left", || reader.left() == 3);
        assert!(
            drain(&mut rx).is_empty(),
            "the refused job never ran when a thread left"
        );
    }

    #[test]
    fn freed_comes_only_from_a_stuck_or_held_back_entry() {
        let fs = HeldFs::new(&["/srv/x", "/mnt/nas/a", "/mnt/nas/b", "/mnt/disk/a"]);
        let (reader, clock) = reader(&fs);
        // A quiet read.
        let (tx, mut rx) = unbounded();
        let _quiet = reader.request(job(1, "/srv/x"), tx);
        wait_until("the quiet read left", || reader.left() == 1);
        assert!(freed(&drain(&mut rx)).is_empty(), "a quiet read says nothing");
        // A young holder that held another read back.
        fs.hold("/mnt/nas");
        let (tx_a, mut rx_a) = unbounded();
        let _a = reader.request(job(2, "/mnt/nas/a"), tx_a);
        fs.wait_blocked("/mnt/nas", 1);
        let (tx_b, mut rx_b) = unbounded();
        let _b = reader.request(job(3, "/mnt/nas/b"), tx_b);
        wait_until("the second waits", || reader.slots()[0].held_back);
        fs.release("/mnt/nas");
        wait_until("both left", || reader.left() == 3);
        assert_eq!(freed(&drain(&mut rx_a)), [Path::new("/mnt/nas")], "it held a read back");
        assert!(
            freed(&drain(&mut rx_b)).is_empty(),
            "the one that waited held nobody back"
        );
        // An entry that grew stuck with nobody waiting.
        fs.hold("/mnt/disk");
        let (tx_c, mut rx_c) = unbounded();
        let _c = reader.request(job(4, "/mnt/disk/a"), tx_c);
        fs.wait_blocked("/mnt/disk", 1);
        advance(&reader, &clock, LIMITS.stuck);
        fs.release("/mnt/disk");
        wait_until("it left", || reader.left() == 4);
        assert_eq!(freed(&drain(&mut rx_c)), [Path::new("/mnt/disk")], "it was stuck");
    }

    #[test]
    fn a_refused_read_sends_no_freed_and_no_loop_starts() {
        let fs = HeldFs::new(&["/mnt/nas/a"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, _rx) = unbounded();
        let _held = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        advance(&reader, &clock, LIMITS.stuck);
        // A reopened chooser asks again and again: each ask is refused at once and its thread leaves quietly.
        let (tx2, mut rx2) = unbounded();
        for serial in 2..=6u64 {
            let _ticket = reader.request(job(serial, "/mnt/nas/a"), tx2.clone());
            wait_until("the refused thread left", || reader.left() == (serial - 1) as usize);
            assert_eq!(reader.slots().len(), 1, "one thread, the held one");
        }
        let answers = drain(&mut rx2);
        assert_eq!(answers.len(), 5, "{answers:?}");
        assert!(answers.iter().all(|a| listing_of(a).1 == &Listing::Stuck));
        fs.release("/mnt/nas");
        wait_until("the held thread left", || reader.left() == 6);
        assert!(drain(&mut rx2).is_empty(), "nothing more to the reopened chooser");
    }

    #[test]
    fn a_late_answer_is_delivered_after_stuck() {
        let fs = HeldFs::new(&["/mnt/nas/a/x"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let mut desk = Desk::default();
        let (tx, mut rx) = unbounded();
        let job = desk.ask(job(1, "/mnt/nas/a"), false).unwrap();
        desk.started(1, reader.request(job, tx));
        fs.wait_blocked("/mnt/nas", 1);
        advance(&reader, &clock, LIMITS.stuck);
        assert_eq!(desk.stuck(1), Some(BROWSE_STUCK));
        assert!(desk.slow(1), "the serial stays open");
        fs.release("/mnt/nas");
        let (answer, lines) = desk.take(next(&mut rx)).expect("the late listing passes");
        assert_eq!(names(listing_of(&answer).1), ["x"]);
        assert_eq!(lines, [BROWSE_LATE, "[project] browse read 1"]);
        assert!(!desk.slow(1), "a listing closes the serial");
        wait_until("it left", || reader.left() == 1);
    }

    #[test]
    fn a_late_answer_with_no_receiver_is_dropped_without_panic() {
        let fs = HeldFs::new(&["/mnt/nas/a/x"]);
        let (reader, clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, rx) = unbounded();
        let _ticket = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        advance(&reader, &clock, LIMITS.stuck);
        drop(rx);
        fs.release("/mnt/nas");
        wait_until("it left", || reader.left() == 1);
        assert_eq!(reader.finished(), 1, "the thread ran to its end");
    }

    #[test]
    fn later_stops_at_the_threads_stuck_age() {
        let dirs: Vec<String> = (0..10).map(|n| format!("/mnt/disk/p/f{n}")).collect();
        let fs = HeldFs::new(&dirs.iter().map(String::as_str).collect::<Vec<_>>());
        let (reader, clock) = reader(&fs);
        fs.hold_path("/mnt/disk/p/f0/.git");
        let (tx, mut rx) = unbounded();
        let _ticket = reader.request(job(1, "/mnt/disk/p"), tx);
        let answer = next(&mut rx);
        assert_eq!(
            names(listing_of(&answer).1).len(),
            10,
            "the names come before the marks"
        );
        fs.wait_blocked("/mnt/disk/p/f0/.git", 1);
        advance(&reader, &clock, LIMITS.stuck);
        fs.release_path("/mnt/disk/p/f0/.git");
        wait_until("it left", || reader.left() == 1);
        let git_calls = fs.every().iter().filter(|(_, p)| p.ends_with(".git")).count();
        assert_eq!(git_calls, 1, "no mark is looked for past the thread's stuck age");
        assert!(drain(&mut rx).iter().all(|a| !matches!(a, ReadAnswer::Later { .. })));
    }

    #[test]
    fn later_marks_local_folders_while_young() {
        let fs = HeldFs::new(&["/mnt/disk/p/one/.git", "/mnt/disk/p/two", "/mnt/nas/q/three/.git"]);
        let (reader, _clock) = reader(&fs);
        let (tx, mut rx) = unbounded();
        let _local = reader.request(job(1, "/mnt/disk/p"), tx.clone());
        let _remote = reader.request(job(2, "/mnt/nas/q"), tx);
        wait_until("both left", || reader.left() == 2);
        let later: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter(|a| matches!(a, ReadAnswer::Later { .. }))
            .collect();
        assert_eq!(
            later,
            [ReadAnswer::Later {
                serial: 1,
                git: vec!["one".into()],
                links: vec![],
            }],
            "a network mount gets no marks"
        );
    }

    #[test]
    fn the_table_lock_is_not_held_during_a_call() {
        let fs = HeldFs::new(&["/mnt/nas/a"]);
        let (reader, _clock) = reader(&fs);
        fs.hold("/mnt/nas");
        let (tx, _rx) = unbounded();
        let _ticket = reader.request(job(1, "/mnt/nas/a"), tx);
        fs.wait_blocked("/mnt/nas", 1);
        assert!(
            reader.shared.table.try_lock().is_ok(),
            "a thread in a call holds no lock"
        );
        fs.release("/mnt/nas");
        wait_until("it left", || reader.left() == 1);
    }

    #[test]
    fn threads_are_named_project_read() {
        let fs = HeldFs::new(&["/srv/x/one"]);
        let (reader, _clock) = reader(&fs);
        let (tx, mut rx) = unbounded();
        let _ticket = reader.request(job(1, "/srv/x"), tx);
        assert_eq!(names(listing_of(&next(&mut rx)).1), ["one"]);
        wait_until("it left", || reader.left() == 1);
        let every = fs.every();
        assert!(!every.is_empty());
        assert!(every.iter().all(|(name, _)| name == "project-read"), "{every:?}");
    }

    #[test]
    fn debounced_jobs_wait_and_a_newer_one_replaces_them() {
        let mut desk = Desk::default();
        assert_eq!(desk.ask(job(1, "/a"), true), None, "typing waits");
        assert_eq!(desk.ask(job(2, "/ab"), true), None);
        assert_eq!(desk.due(1), None, "the older job was replaced");
        assert!(!desk.slow(2), "nothing is read yet, so nothing is slow");
        assert_eq!(desk.due(2), Some(job(2, "/ab")));
        assert_eq!(desk.due(2), None, "a job starts once");
        assert_eq!(desk.ask(job(3, "/abc"), true), None);
        assert_eq!(
            desk.ask(job(4, "/x"), false),
            Some(job(4, "/x")),
            "a move reads at once"
        );
        assert_eq!(desk.due(3), None, "and drops the typed job still waiting");
        let stale = ReadAnswer::Listing {
            serial: 2,
            dir: "/ab".into(),
            listing: Listing::Missing,
        };
        assert_eq!(desk.take(stale), None, "an answer to a superseded serial is dropped");
    }

    fn entry(name: &str) -> Entry {
        Entry {
            raw: name.into(),
            name: name.into(),
            kind: EntryKind::Dir { mount_point: false },
            physical: None,
        }
    }

    #[test]
    fn the_browse_log_lines_are_exact_and_carry_no_path() {
        let mut desk = Desk::default();
        let mut lines = Vec::new();
        desk.ask(job(1, "/home/secret"), false);
        lines.extend(desk.stuck(1).map(str::to_owned));
        let read = Listing::Read {
            dir: "/home/secret".into(),
            entries: vec![entry("private-name"), entry("b")],
            capped: None,
            exact: None,
        };
        let answer = ReadAnswer::Listing {
            serial: 1,
            dir: "/home/secret".into(),
            listing: read,
        };
        lines.extend(desk.take(answer).unwrap().1);
        let later = ReadAnswer::Later {
            serial: 1,
            git: vec!["private-name".into()],
            links: vec![],
        };
        assert_eq!(desk.take(later.clone()), Some((later, vec![])), "marks log nothing");
        lines.extend(
            desk.take(ReadAnswer::Freed {
                mount: "/mnt/secret".into(),
            })
            .unwrap()
            .1,
        );
        assert_eq!(
            lines,
            [
                "[project] browse stuck",
                "[project] browse late",
                "[project] browse read 2",
                "[project] browse freed"
            ]
        );
        // A folder that could not be listed was still read; a refusal read nothing.
        desk.ask(job(2, "/home/secret/gone"), false);
        let missing = ReadAnswer::Listing {
            serial: 2,
            dir: "/home/secret/gone".into(),
            listing: Listing::Missing,
        };
        assert_eq!(desk.take(missing).unwrap().1, ["[project] browse read 0"]);
        for (serial, refused) in [(3, Listing::Stuck), (4, Listing::TooManyStuck)] {
            desk.ask(job(serial, "/home/secret"), false);
            let answer = ReadAnswer::Listing {
                serial,
                dir: "/home/secret".into(),
                listing: refused,
            };
            assert_eq!(desk.take(answer).unwrap().1, Vec::<String>::new());
        }
        for line in &lines {
            assert!(
                !line.contains('/') && !line.contains("secret") && !line.contains("private"),
                "{line}"
            );
        }
    }

    /// Answers reach the main loop through a channel a `spawn_future_local` future awaits, and the carriers' timers
    /// are one-shot. The typed path's poll is the one repeating timer left.
    #[test]
    fn answers_reach_the_loop_through_spawn_future_local() {
        use eitri_core::source_scan::{code_only, functions, read_required, without};
        let host = code_only(&read_required("shell/src/project_host.rs").text);
        let host = &host[..host.find("mod tests").expect("the test module")];
        let [poll] = functions(host, "arm")[..] else {
            panic!("one poll for the typed path");
        };
        let rest = without(host, &[poll]);
        for banned in ["timeout_add_local(", "try_iter(", "try_recv("] {
            assert!(!rest.contains(banned), "{banned} outside the typed path's poll");
        }
        let [listen] = functions(host, "listen")[..] else {
            panic!("one listen");
        };
        assert!(listen.contains("glib::spawn_future_local") && listen.contains(".recv().await"));
        for name in ["list", "open_with"] {
            let [body] = functions(host, name)[..] else {
                panic!("one {name}");
            };
            assert!(body.contains("unbounded_send(") && body.contains("listen("), "{name}");
            assert!(!body.contains("arm("), "{name} needs no poll");
        }
        let [start] = functions(host, "start_read")[..] else {
            panic!("one start_read");
        };
        assert!(start.contains(".request(") && start.contains("listen("));
        assert!(
            start.contains("timeout_add_local_once(READ_SLOW") && start.contains("timeout_add_local_once(READ_STUCK")
        );
        let [read] = functions(host, "read")[..] else {
            panic!("one read");
        };
        assert!(read.contains("timeout_add_local_once(READ_DEBOUNCE"));

        let launch = code_only(&read_required("shell/src/choose_project.rs").text);
        let launch = &launch[..launch.find("mod tests").expect("the test module")];
        for banned in ["timeout_add_local(", "try_recv(", "POLL_EVERY"] {
            assert!(!launch.contains(banned), "{banned} in the launch window's carrier");
        }
        let [start] = functions(launch, "start")[..] else {
            panic!("one start");
        };
        assert!(
            start.contains("glib::spawn_future_local") && start.contains(".recv().await"),
            "the child's exit"
        );
        assert!(
            launch.contains("self.workers.read("),
            "the launch window's reads go to the reader"
        );
    }
}
