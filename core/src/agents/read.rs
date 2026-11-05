//! Every process's rows merged into one view: [`scan_once`] reads it once, a [`Reader`] keeps it current.
//!
//! The reader's thread polls one set of descriptors: an inotify watch on `<control dir>/agents`, one `pidfd` per
//! live writer, and any descriptor the caller adds. A publish is a rename, so it wakes the reader once and only the
//! file that changed is read again. A writer's death changes no file; its `pidfd` turning readable (or, without
//! one, its lock found free) is how the reader learns of it, and the reader that takes the dead writer's lock
//! removes what it left.
//!
//! A row is shown once per key. A tab moving between windows can be in two files at once (the target published
//! before the source caught up): the row without `moving` is shown. A row that leaves its file with a departure
//! saying `moved` is held up to the move hold for its key to appear in another file, so a face keeping places
//! by key never frees the slot; when the key appears, `gone` (`moved`) goes out and the new row right after it.
//! A row that leaves without a departure, because its writer ended, is `gone` with `window`.
//!
//! The projects with a running window travel only in a whole view: when they change, the batch's `gone` changes
//! go out first, with their reasons, and a `reset` follows in place of its `row` changes.
//!
//! The shared selection and round (`selection.json`, `round.json`, see [`super::shared`]) are read through the
//! same watch: each whole view carries them, and a change to either goes out as `selection` or `round` after the
//! batch's row changes. One that cannot be read (a link, another user's, a newer schema's) is logged once like
//! any ignored file and reads as none.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent::private_fs::{self, euid};

use super::liveness::{self, Alive};
use super::publish::{agents_dir, is_instance, open_own_dir};
use super::shared::{self, Round, Selection, SharedName, SharedRead, ROUND_FILE, SELECTION_FILE};
use super::wire::{parse_process_file, WireError};
use super::{
    Change, GoneWhy, ProcessFile, ProjectRef, Row, View, LOCK_POLL_MS, MAX_FILE_BYTES, MAX_ROWS_PER_FILE, MOVE_HOLD_MS,
    SCHEMA_V,
};

/// What a [`Reader`] hands its caller, on the reader's own thread. Nearly every event is a change, so the change
/// is not boxed to spare the rare error its size.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderEvent {
    Change(Change),
    /// The reader cannot go on (the directory went, the watch failed); nothing follows it.
    Error(String),
}

/// What one turn of the reader's loop did, for a test to follow without timing anything.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    /// Instances whose files were opened and read.
    pub read: Vec<String>,
    /// Ids of rows held for a move to land.
    pub held: Vec<String>,
    /// Writers found dead.
    pub buried: Vec<String>,
    /// Those of them whose files this reader removed.
    pub cleaned: Vec<String>,
    /// Changes sent.
    pub changed: usize,
}

#[doc(hidden)]
pub type LogHook = Box<dyn FnMut(&str) + Send>;
#[doc(hidden)]
pub type BatchHook = Box<dyn FnMut(&Batch) + Send>;

/// What a test replaces. The defaults are what a reader runs with.
#[doc(hidden)]
pub struct ReaderHooks {
    /// Whether writers are watched through a `pidfd`; without, their locks are polled every `LOCK_POLL_MS`.
    pub pidfd: bool,
    /// The owner a file is judged by, given its path and the owner it has.
    pub owner: fn(&Path, u32) -> u32,
    /// How long a row that left for another window waits for its key to appear there.
    pub move_hold: Duration,
    /// Where the reader's lines go.
    pub log: LogHook,
    /// Set to have the next read of the watch taken as an overflow.
    pub overflow: Option<Arc<AtomicBool>>,
    pub on_batch: Option<BatchHook>,
}

fn owner_as_is(_: &Path, uid: u32) -> u32 {
    uid
}

impl Default for ReaderHooks {
    fn default() -> Self {
        Self {
            pidfd: true,
            owner: owner_as_is,
            move_hold: Duration::from_millis(MOVE_HOLD_MS),
            log: Box::new(|line| eprintln!("{line}")),
            overflow: None,
            on_batch: None,
        }
    }
}

fn check_schema(schema: u32) -> io::Result<()> {
    if schema == 0 || schema > SCHEMA_V {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("agents schema {schema} is not one this build reads (1 to {SCHEMA_V})"),
        ));
    }
    Ok(())
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A name the reader looks at: ends in `.json` and is not a writer's temporary file.
fn is_json_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.ends_with(b".json") && !bytes.starts_with(b".")
}

/// The instance a file belongs to, when its name is exactly `<16 lowercase hex>.json`.
fn instance_of(name: &OsStr) -> Option<&str> {
    let stem = name.to_str()?.strip_suffix(".json")?;
    is_instance(stem).then_some(stem)
}

/// A directory handle as a path, so a listing, a read or a watch reaches the very directory that was judged.
fn proc_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

enum Found {
    File(ProcessFile),
    Missing,
    Ignored(String),
}

/// Reads `<dir>/<instance>.json`: a regular file of this user (by `owner`), no link, at most `MAX_FILE_BYTES`,
/// of a schema up to `schema`, naming the instance its file name does. The flag says whether its bytes were read.
fn read_file(dir: &Path, instance: &str, schema: u32, owner: fn(&Path, u32) -> u32) -> (Found, bool) {
    let path = dir.join(format!("{instance}.json"));
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Found::Missing, false),
        Err(e) => return (Found::Ignored(format!("cannot be looked at: {e}")), false),
    };
    let refused = if meta.file_type().is_symlink() {
        Some("a symbolic link".to_owned())
    } else if !meta.is_file() {
        Some("not a regular file".to_owned())
    } else if owner(&path, meta.uid()) != euid() {
        Some("another user's file".to_owned())
    } else if meta.len() > MAX_FILE_BYTES as u64 {
        Some(format!("{} bytes, over the {MAX_FILE_BYTES}-byte limit", meta.len()))
    } else {
        None
    };
    if let Some(why) = refused {
        return (Found::Ignored(why), false);
    }
    let text = match private_fs::read_private_nofollow_capped(&path, MAX_FILE_BYTES as u64) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Found::Missing, false),
        Err(e) => return (Found::Ignored(format!("cannot be read: {e}")), false),
    };
    let found = match parse_process_file(text.as_bytes()) {
        Ok(file) if file.v > schema => Found::Ignored(format!("schema {} is newer than {schema}", file.v)),
        Ok(file) if file.instance != instance => Found::Ignored("it names another window".to_owned()),
        Ok(file) => Found::File(file),
        Err(WireError::NewerMajor(v)) => Found::Ignored(format!("schema {v} is newer than {schema}")),
        Err(e) => Found::Ignored(e.to_string()),
    };
    (found, true)
}

/// A row as shown, and the writer it came from.
#[derive(Debug, Clone)]
struct Shown {
    row: Row,
    instance: String,
}

/// One row per key from `files` (in instance order). Of two rows with one key, the one not `moving` wins, then
/// the one already shown, then the first.
fn merge<'a>(
    files: impl Iterator<Item = (&'a String, &'a ProcessFile)>,
    shown: &BTreeMap<String, Shown>,
) -> BTreeMap<String, Shown> {
    let rank = |r: &Row| (!r.moving, shown.contains_key(&r.id));
    let mut by_key: HashMap<String, Shown> = HashMap::new();
    for (instance, file) in files {
        for row in &file.rows {
            // A row without a key stands alone.
            let key = if row.key.is_empty() {
                format!("\0{}", row.id)
            } else {
                row.key.clone()
            };
            let better = by_key.get(&key).is_none_or(|have| rank(row) > rank(&have.row));
            if better {
                let candidate = Shown {
                    row: row.clone(),
                    instance: instance.clone(),
                };
                by_key.insert(key, candidate);
            }
        }
    }
    let mut by_id = BTreeMap::new();
    for shown in by_key.into_values() {
        by_id.entry(shown.row.id.clone()).or_insert(shown);
    }
    by_id
}

/// The order a view's rows come in: project name and key, window name and instance, tab number.
fn order(r: &Row) -> (&str, &str, &str, &str, u16, &str) {
    (
        &r.project.name,
        &r.project.key,
        &r.window.name,
        &r.window.instance,
        r.number,
        &r.id,
    )
}

/// Every project the files name, once each, by name and key.
fn projects_of<'a>(files: impl Iterator<Item = &'a ProcessFile>) -> Vec<ProjectRef> {
    let mut projects: Vec<ProjectRef> = Vec::new();
    for file in files {
        if !file.project.key.is_empty() && !projects.iter().any(|p| p.key == file.project.key) {
            projects.push(file.project.clone());
        }
    }
    projects.sort_by(|a, b| (&a.name, &a.key).cmp(&(&b.name, &b.key)));
    projects
}

fn view_of<'a>(rows: impl Iterator<Item = &'a Row>, files: impl Iterator<Item = &'a ProcessFile>) -> View {
    let mut rows: Vec<Row> = rows.cloned().collect();
    rows.sort_by(|a, b| order(a).cmp(&order(b)));
    View {
        at: wall_clock_ms(),
        projects: projects_of(files),
        rows,
        ..View::default()
    }
}

/// How old a lock with no file beside it must be before it is taken for one whose writer ended before its first
/// publish: a writer makes its lock a moment before it takes it.
const ORPHAN_LOCK_AGE: Duration = Duration::from_millis(LOCK_POLL_MS);

/// Removes, through `clean_dead`, each `<instance>.lock` in `names` with no `<instance>.json` beside it that is
/// older than `ORPHAN_LOCK_AGE`: what a writer that ended before it published anything left behind.
fn sweep_orphan_locks(dir: &File, control: &File, path: &Path, names: &BTreeSet<OsString>) {
    for name in names {
        let Some(instance) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".lock"))
            .filter(|i| is_instance(i))
        else {
            continue;
        };
        if names.contains(OsStr::new(&format!("{instance}.json"))) {
            continue;
        }
        let old = std::fs::symlink_metadata(path.join(name))
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= ORPHAN_LOCK_AGE));
        if old {
            let _ = liveness::clean_dead(dir, control, instance);
        }
    }
}

/// The view as it is now, read once: what `prefix u` and `eitri agents --json` show. A dead writer found on the
/// way is cleaned up as a watching reader would. No agents directory yet is an empty view, and nothing is made.
pub fn scan_once(control_dir: &Path, schema: u32) -> io::Result<View> {
    check_schema(schema)?;
    let dir = match open_own_dir(&agents_dir(control_dir)) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(view_of(std::iter::empty(), std::iter::empty()));
        }
        Err(e) => return Err(e),
    };
    let control = open_own_dir(control_dir)?;
    let path = proc_path(&dir);
    let names = std::fs::read_dir(&path)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<io::Result<BTreeSet<OsString>>>()?;
    sweep_orphan_locks(&dir, &control, &path, &names);
    let mut files = BTreeMap::new();
    for name in &names {
        let Some(instance) = instance_of(name) else {
            continue;
        };
        let (Found::File(file), _) = read_file(&path, instance, schema, owner_as_is) else {
            continue;
        };
        let lock = path.join(format!("{instance}.lock"));
        match liveness::check(file.pid, file.pid_start, &lock) {
            Alive::Yes(_) => {
                files.insert(instance.to_owned(), file);
            }
            Alive::No => {
                let _ = liveness::clean_dead(&dir, &control, instance);
            }
        }
    }
    let rows = merge(files.iter(), &BTreeMap::new());
    let mut view = view_of(rows.values().map(|s| &s.row), files.values());
    if let SharedRead::Found(selection) = shared::read_selection_at(&path.join(SELECTION_FILE), schema, owner_as_is) {
        view.selection = Some(selection);
    }
    if let SharedRead::Found(round) = shared::read_round_at(&path.join(ROUND_FILE), schema, owner_as_is) {
        view.round = Some(round);
    }
    Ok(view)
}

const WATCHED: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;
const DIRECTORY_WENT: u32 = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED | libc::IN_UNMOUNT;
/// `struct inotify_event` without its name: `wd`, `mask`, `cookie`, `len`, four bytes each.
const EVENT_HEADER: usize = 16;
/// Room for at least one event with the longest name.
const EVENT_BUFFER: usize = 4096;

fn make_inotify() -> io::Result<OwnedFd> {
    // SAFETY: `inotify_init1` takes no pointers; a negative return is an error.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn add_watch(inotify: &OwnedFd, dir: &File) -> io::Result<()> {
    let path = CString::new(proc_path(dir).as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `path` is NUL-terminated and outlives the call, and the kernel does not keep it.
    if unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), WATCHED) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn make_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: `eventfd` takes no pointers; a negative return is an error.
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// What the caller's handle and the reader's thread share.
struct Shared {
    stop: AtomicBool,
    extra: Mutex<Vec<OwnedFd>>,
    /// An eventfd: written to wake the thread, which then looks at `stop` and `extra`.
    wake: OwnedFd,
}

impl Shared {
    fn wake(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes the eight bytes of `one`, which lives for the call. A full counter cannot happen here
        // (the thread drains it), and a failed wake leaves the thread to its next event.
        unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    fn drain(&self) {
        let mut count = [0u8; 8];
        // SAFETY: reads at most eight bytes into `count`, which lives for the call.
        unsafe { libc::read(self.wake.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
    }
}

/// Watches every process's agents file and hands each change to the caller on its own thread. Its methods do
/// no disk work, but [`Reader::start`] does (it makes and judges the directory), and stopping joins the thread.
pub struct Reader {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Reader {
    /// Makes `<control dir>/agents` if needed (0700, judged as the publisher judges it), watches it and starts
    /// the thread, which sends `Reset` with the whole view first and then every change. `schema` is the newest
    /// schema the caller understands: files of a newer one are skipped with one log line each.
    pub fn start(control_dir: &Path, schema: u32, tx: impl Fn(ReaderEvent) + Send + 'static) -> io::Result<Reader> {
        Self::start_with(control_dir, schema, tx, ReaderHooks::default())
    }

    #[doc(hidden)]
    pub fn start_with(
        control_dir: &Path,
        schema: u32,
        tx: impl Fn(ReaderEvent) + Send + 'static,
        hooks: ReaderHooks,
    ) -> io::Result<Reader> {
        check_schema(schema)?;
        crate::panel_control::ensure_dir(control_dir)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
        let path = agents_dir(control_dir);
        private_fs::create_private_dir_all(&path, control_dir)?;
        let dir = open_own_dir(&path)?;
        let control = open_own_dir(control_dir)?;
        let inotify = make_inotify()?;
        add_watch(&inotify, &dir)?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            extra: Mutex::new(Vec::new()),
            wake: make_eventfd()?,
        });
        let watch = Watch {
            dir_path: proc_path(&dir),
            dir,
            control,
            inotify,
            shared: shared.clone(),
            schema,
            hooks,
            tx: Box::new(tx),
            extra: Vec::new(),
            entries: BTreeMap::new(),
            left: HashMap::new(),
            dead: HashSet::new(),
            logged: HashSet::new(),
            noted_truncated: HashSet::new(),
            shown: BTreeMap::new(),
            held: HashMap::new(),
            projects: Vec::new(),
            selection: None,
            round: None,
            sent_selection: None,
            sent_round: None,
            next_lock_poll: None,
            batch: Batch::default(),
        };
        let thread = std::thread::Builder::new()
            .name("eitri-agents-read".into())
            .spawn(move || watch.run())?;
        Ok(Reader {
            shared,
            thread: Some(thread),
        })
    }

    /// Polls `fd` with the reader's other descriptors: when it reports a hang-up or an error (the CLI's stdout
    /// whose reader went), the reader ends and drops the caller's sender. It is not polled for input, so a
    /// terminal's keystrokes never end it.
    pub fn add_fd(&self, fd: OwnedFd) {
        self.shared
            .extra
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(fd);
        self.shared.wake();
    }

    /// Ends the thread and waits for it.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            // Dropped from inside the caller's own callback: the thread ends by itself once it returns.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

/// A live writer's file, and the descriptor that turns readable when it exits.
struct Entry {
    file: ProcessFile,
    pidfd: Option<OwnedFd>,
}

struct Watch {
    dir: File,
    dir_path: PathBuf,
    control: File,
    inotify: OwnedFd,
    shared: Arc<Shared>,
    schema: u32,
    hooks: ReaderHooks,
    tx: Box<dyn Fn(ReaderEvent) + Send>,
    extra: Vec<OwnedFd>,
    /// Live writers by instance.
    entries: BTreeMap<String, Entry>,
    /// Files of writers that left in this batch, for the departures they named.
    left: HashMap<String, ProcessFile>,
    /// Writers found dead, whose files are not read again.
    dead: HashSet<String>,
    /// Names whose being ignored has been logged, until they are read or go.
    logged: HashSet<String>,
    noted_truncated: HashSet<String>,
    /// Rows as last handed out, by id; held rows among them.
    shown: BTreeMap<String, Shown>,
    held: HashMap<String, Instant>,
    /// The projects as last handed out.
    projects: Vec<ProjectRef>,
    /// The shared selection and round as last read, and as last handed out.
    selection: Option<Selection>,
    round: Option<Round>,
    sent_selection: Option<Selection>,
    sent_round: Option<Round>,
    next_lock_poll: Option<Instant>,
    batch: Batch,
}

impl Watch {
    fn run(mut self) {
        if let Err(e) = self.rescan() {
            return self.fail(format!("could not read the agents directory: {e}"));
        }
        self.reset();
        self.finish_batch();
        loop {
            self.extra.extend(
                self.shared
                    .extra
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .drain(..),
            );
            let mut fds = vec![
                libc::pollfd {
                    fd: self.shared.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            fds.extend(self.extra.iter().map(|fd| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: 0,
                revents: 0,
            }));
            let watched: Vec<(String, RawFd)> = self
                .entries
                .iter()
                .filter_map(|(instance, e)| e.pidfd.as_ref().map(|fd| (instance.clone(), fd.as_raw_fd())))
                .collect();
            let pids_from = fds.len();
            fds.extend(watched.iter().map(|(_, fd)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            }));
            self.arm_lock_poll(Instant::now());
            let timeout = self.timeout_ms(Instant::now());
            // SAFETY: `fds` holds `fds.len()` initialised entries and lives for the call.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return self.fail(format!("could not wait for changes: {e}"));
            }
            if fds[0].revents != 0 {
                self.shared.drain();
            }
            if self.shared.stop.load(Ordering::SeqCst) {
                return;
            }
            if fds[2..pids_from].iter().any(|fd| fd.revents != 0) {
                return;
            }
            let mut dirty = BTreeSet::new();
            let mut overflow = false;
            if fds[1].revents != 0 {
                match self.read_events(&mut dirty) {
                    Ok(lost) => overflow = lost,
                    Err(why) => return self.fail(why),
                }
            }
            for ((instance, _), fd) in watched.iter().zip(&fds[pids_from..]) {
                if fd.revents != 0 {
                    self.bury(instance);
                }
            }
            self.poll_locks(Instant::now());
            if overflow {
                if let Err(e) = self.rescan() {
                    return self.fail(format!("could not read the agents directory: {e}"));
                }
                self.reset();
            } else {
                for name in dirty {
                    self.refresh(&name);
                }
                self.diff();
            }
            self.finish_batch();
        }
    }

    fn fail(&mut self, why: String) {
        (self.tx)(ReaderEvent::Error(why));
    }

    fn timeout_ms(&self, now: Instant) -> libc::c_int {
        let due = self.held.values().copied().chain(self.next_lock_poll).min();
        match due {
            None => -1,
            Some(due) => {
                let wait = due.saturating_duration_since(now);
                // Rounded up, so the loop never wakes just before what it waits for.
                let ms = wait.as_millis() + u128::from(wait.subsec_nanos() % 1_000_000 != 0);
                libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
            }
        }
    }

    /// Reads the watch until it is empty, noting the names that changed. `Ok(true)` when events were lost.
    fn read_events(&mut self, dirty: &mut BTreeSet<OsString>) -> Result<bool, String> {
        let mut lost = false;
        let mut buffer = [0u8; EVENT_BUFFER];
        loop {
            // SAFETY: reads at most `buffer.len()` bytes into `buffer`, which lives for the call.
            let read = unsafe { libc::read(self.inotify.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            let Ok(read) = usize::try_from(read) else {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => break,
                    _ => return Err(format!("could not read the agents directory's watch: {e}")),
                }
            };
            if read == 0 {
                break;
            }
            let mut events = &buffer[..read];
            while events.len() >= EVENT_HEADER {
                let word = |at: usize| [events[at], events[at + 1], events[at + 2], events[at + 3]];
                let mask = u32::from_ne_bytes(word(4));
                let name_len = u32::from_ne_bytes(word(12)) as usize;
                let end = EVENT_HEADER.saturating_add(name_len).min(events.len());
                let name = events[EVENT_HEADER..end].split(|b| *b == 0).next().unwrap_or_default();
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    lost = true;
                } else if mask & DIRECTORY_WENT != 0 {
                    return Err("the agents directory went away".to_owned());
                } else if !name.is_empty() {
                    dirty.insert(OsStr::from_bytes(name).to_owned());
                }
                events = &events[end..];
            }
        }
        if let Some(flag) = &self.hooks.overflow {
            lost |= flag.swap(false, Ordering::SeqCst);
        }
        Ok(lost)
    }

    /// Reads every file in the directory and every writer known, after which the view is whole again.
    fn rescan(&mut self) -> io::Result<()> {
        let mut names: BTreeSet<OsString> = BTreeSet::new();
        for entry in std::fs::read_dir(&self.dir_path)? {
            names.insert(entry?.file_name());
        }
        sweep_orphan_locks(&self.dir, &self.control, &self.dir_path, &names);
        names.extend(self.entries.keys().map(|i| OsString::from(format!("{i}.json"))));
        // Read even when gone, so a shared file removed while events were lost reads as none.
        names.extend([SELECTION_FILE, ROUND_FILE].map(OsString::from));
        for name in names {
            self.refresh(&name);
        }
        Ok(())
    }

    /// Reads one name again and brings the writers up to date with it.
    fn refresh(&mut self, name: &OsStr) {
        if !is_json_name(name) {
            return;
        }
        let shown_name = name.to_string_lossy().into_owned();
        if let Some(which) = SharedName::of(name) {
            return self.refresh_shared(which, &shown_name);
        }
        let Some(instance) = instance_of(name).map(str::to_owned) else {
            return self.ignore(&shown_name, "not an agents file name", None);
        };
        let (found, read) = read_file(&self.dir_path, &instance, self.schema, self.hooks.owner);
        if read {
            self.batch.read.push(instance.clone());
        }
        match found {
            Found::Missing => {
                self.logged.remove(&shown_name);
                self.dead.remove(&instance);
                self.forget(&instance);
            }
            Found::Ignored(why) => self.ignore(&shown_name, &why, Some(&instance)),
            Found::File(file) => {
                self.logged.remove(&shown_name);
                if self.dead.contains(&instance) {
                    return;
                }
                if file.truncated && self.noted_truncated.insert(instance.clone()) {
                    (self.hooks.log)(&format!(
                        "[agents] window {instance} has more tabs than its file holds; showing its first \
                         {MAX_ROWS_PER_FILE}"
                    ));
                }
                if let Some(entry) = self.entries.get_mut(&instance) {
                    entry.file = file;
                    return;
                }
                let lock = self.dir_path.join(format!("{instance}.lock"));
                match liveness::check_with(file.pid, file.pid_start, &lock, self.hooks.pidfd) {
                    Alive::Yes(pidfd) => {
                        self.entries.insert(instance, Entry { file, pidfd });
                    }
                    Alive::No => {
                        self.left.insert(instance.clone(), file);
                        self.bury(&instance);
                    }
                }
            }
        }
    }

    /// Reads a shared file again. What cannot be read is logged as an ignored file and reads as none.
    fn refresh_shared(&mut self, which: SharedName, name: &str) {
        let path = self.dir_path.join(name);
        match which {
            SharedName::Selection => {
                let found = shared::read_selection_at(&path, self.schema, self.hooks.owner);
                self.selection = self.settle(found, name);
            }
            SharedName::Round => {
                let found = shared::read_round_at(&path, self.schema, self.hooks.owner);
                self.round = self.settle(found, name);
            }
        }
    }

    fn settle<T>(&mut self, found: SharedRead<T>, name: &str) -> Option<T> {
        match found {
            SharedRead::Found(value) => {
                self.logged.remove(name);
                Some(value)
            }
            SharedRead::Missing => {
                self.logged.remove(name);
                None
            }
            SharedRead::Ignored(why) => {
                self.ignore(name, &why, None);
                None
            }
        }
    }

    /// A file that is not read: logged once until it is read or goes, and its writer's rows leave.
    fn ignore(&mut self, name: &str, why: &str, instance: Option<&str>) {
        if self.logged.insert(name.to_owned()) {
            (self.hooks.log)(&format!("[agents] ignored {name}: {why}"));
        }
        if let Some(instance) = instance {
            self.forget(instance);
        }
    }

    fn forget(&mut self, instance: &str) {
        if let Some(entry) = self.entries.remove(instance) {
            self.left.insert(instance.to_owned(), entry.file);
        }
    }

    /// The writer `instance` has ended: its rows leave, and its files go if this reader takes its lock first.
    fn bury(&mut self, instance: &str) {
        self.forget(instance);
        self.dead.insert(instance.to_owned());
        self.batch.buried.push(instance.to_owned());
        match liveness::clean_dead(&self.dir, &self.control, instance) {
            Ok(true) => self.batch.cleaned.push(instance.to_owned()),
            Ok(false) => {}
            Err(e) => (self.hooks.log)(&format!(
                "[agents] could not remove the files of window {instance}, whose process ended: {e}"
            )),
        }
    }

    /// Writers without a `pidfd` are asked through their locks every `LOCK_POLL_MS`, and only while there are any.
    fn arm_lock_poll(&mut self, now: Instant) {
        if !self.entries.values().any(|e| e.pidfd.is_none()) {
            self.next_lock_poll = None;
        } else if self.next_lock_poll.is_none() {
            self.next_lock_poll = Some(now + Duration::from_millis(LOCK_POLL_MS));
        }
    }

    fn poll_locks(&mut self, now: Instant) {
        if self.next_lock_poll.is_none_or(|due| due > now) {
            return;
        }
        let unwatched: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.pidfd.is_none())
            .map(|(instance, _)| instance.clone())
            .collect();
        for instance in unwatched {
            if !liveness::lock_is_held(&self.dir_path.join(format!("{instance}.lock"))) {
                self.bury(&instance);
            }
        }
        self.next_lock_poll = Some(now + Duration::from_millis(LOCK_POLL_MS));
    }

    fn current(&self) -> BTreeMap<String, Shown> {
        merge(self.entries.iter().map(|(i, e)| (i, &e.file)), &self.shown)
    }

    /// The view of the rows as shown, with the shared selection and round, which are handed out with it.
    fn whole_view(&mut self) -> View {
        let mut view = view_of(
            self.shown.values().map(|s| &s.row),
            self.entries.values().map(|e| &e.file),
        );
        view.selection = self.selection.clone();
        view.round = self.round.clone();
        self.sent_selection = view.selection.clone();
        self.sent_round = view.round.clone();
        self.projects = view.projects.clone();
        view
    }

    /// The whole view again, replacing everything a face holds.
    fn reset(&mut self) {
        self.shown = self.current();
        self.held.clear();
        self.left.clear();
        let view = self.whole_view();
        self.send(Change::Reset { view });
    }

    /// Sends what changed since the rows last handed out.
    fn diff(&mut self) {
        let current = self.current();
        let by_key: HashMap<&str, &str> = current
            .iter()
            .filter(|(_, s)| !s.row.key.is_empty())
            .map(|(id, s)| (s.row.key.as_str(), id.as_str()))
            .collect();
        let now = Instant::now();
        let mut changes = Vec::new();
        let mut sent: HashSet<String> = HashSet::new();
        let leaving: Vec<String> = self
            .shown
            .keys()
            .filter(|id| !current.contains_key(*id))
            .cloned()
            .collect();
        for id in leaving {
            let (key, instance) = {
                let s = &self.shown[&id];
                (s.row.key.clone(), s.instance.clone())
            };
            let gone = |why| Change::Gone {
                id: id.clone(),
                key: key.clone(),
                why,
            };
            // The key is shown from another file: the tab has landed there.
            if let Some(&landed) = by_key.get(key.as_str()) {
                changes.push(gone(GoneWhy::Moved));
                let new = &current[landed];
                if self.shown.get(landed).map(|s| &s.row) != Some(&new.row) {
                    changes.push(Change::Row { row: new.row.clone() });
                }
                sent.insert(landed.to_owned());
                self.held.remove(&id);
                self.shown.remove(&id);
                continue;
            }
            if let Some(due) = self.held.get(&id) {
                if *due <= now {
                    changes.push(gone(GoneWhy::Moved));
                    self.held.remove(&id);
                    self.shown.remove(&id);
                }
                continue;
            }
            let file = self
                .entries
                .get(&instance)
                .map(|e| &e.file)
                .or_else(|| self.left.get(&instance));
            let departed = file.and_then(|f| {
                f.departed
                    .iter()
                    .find(|d| d.id == id || (!key.is_empty() && d.key == key))
            });
            let why = match departed.map(|d| d.why) {
                Some(GoneWhy::Moved) if !self.hooks.move_hold.is_zero() => {
                    self.held.insert(id.clone(), now + self.hooks.move_hold);
                    continue;
                }
                Some(why) => why,
                None if self.entries.contains_key(&instance) => GoneWhy::Closed,
                None => GoneWhy::Window,
            };
            changes.push(gone(why));
            self.shown.remove(&id);
        }
        let mut rows: Vec<&Shown> = current.values().collect();
        rows.sort_by(|a, b| order(&a.row).cmp(&order(&b.row)));
        for s in rows {
            if !sent.contains(&s.row.id) && self.shown.get(&s.row.id).map(|x| &x.row) != Some(&s.row) {
                changes.push(Change::Row { row: s.row.clone() });
            }
        }
        for (id, s) in current {
            self.held.remove(&id);
            self.shown.insert(id, s);
        }
        self.left.clear();
        if self.selection != self.sent_selection {
            self.sent_selection = self.selection.clone();
            changes.push(Change::Selection {
                selection: self.selection.clone(),
            });
        }
        if self.round != self.sent_round {
            self.sent_round = self.round.clone();
            changes.push(Change::Round {
                round: self.round.clone(),
            });
        }
        if projects_of(self.entries.values().map(|e| &e.file)) != self.projects {
            // Rows held for a move stay in the view, so a face keeps their places until they land or time out.
            changes.retain(|change| matches!(change, Change::Gone { .. }));
            let view = self.whole_view();
            changes.push(Change::Reset { view });
        }
        for change in changes {
            self.send(change);
        }
    }

    fn send(&mut self, change: Change) {
        self.batch.changed += 1;
        (self.tx)(ReaderEvent::Change(change));
    }

    fn finish_batch(&mut self) {
        let mut batch = std::mem::take(&mut self.batch);
        batch.held = self.held.keys().cloned().collect();
        batch.held.sort();
        if let Some(hook) = &mut self.hooks.on_batch {
            hook(&batch);
        }
    }
}
