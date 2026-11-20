//! One process's rows, published as `<control dir>/agents/<instance>.json` for every reader of this user.
//!
//! The file is replaced whole (a temporary name, 0600, written, synced, renamed), so a reader never meets a
//! half-written one. All disk work runs on the publisher's own thread: the caller's `offer` only queues the rows,
//! and the thread coalesces offers for `COALESCE_MS` and writes only when the file would change. While the
//! process runs it holds an exclusive lock on `<instance>.lock`; a reader that can take that lock knows the
//! writer is gone. A clean stop removes both files; a crash leaves them for a reader to clean up.

use super::wire::process_file_json;
use super::{
    Departed, GoneWhy, ProcessFile, ProjectRef, Row, WindowRef, COALESCE_MS, DEPARTED_KEEP_MS, MAX_FILE_BYTES,
    MAX_ROWS_PER_FILE, SCHEMA_V,
};
use agent::private_fs::{self, FileIdentity, PrivateLock};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The directory every process's agents file lives in.
pub fn agents_dir(control_dir: &Path) -> PathBuf {
    control_dir.join("agents")
}

/// A fresh name for this process's files: 16 lowercase hex digits, random, never reused.
pub fn new_instance() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_owned()
}

/// An instance name's grammar is spelled where the sockets it names are, a file `eitri remote serve`'s build hash reads.
pub use crate::control_listener::{is_instance, INSTANCE_HEX};

/// The start time of `pid` in clock ticks since boot (field 22 of `/proc/<pid>/stat`), which tells a process
/// apart from a later one given the same pid. `None` where there is no `/proc` or the process is gone.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces or parentheses itself; the fields after the
    // last `)` start with field 3.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Called with each file once it has taken its name.
#[doc(hidden)]
pub type PublishHook = Box<dyn FnMut(&ProcessFile) + Send>;

/// What a test replaces: the clock departures are stamped and aged with, and a look at each file as it is
/// published, called on the publisher's thread after the file has taken its name.
#[doc(hidden)]
pub struct PublisherHooks {
    pub clock: fn() -> u64,
    pub on_publish: Option<PublishHook>,
}

impl Default for PublisherHooks {
    fn default() -> Self {
        Self {
            clock: wall_clock_ms,
            on_publish: None,
        }
    }
}

enum Msg {
    Offer(Vec<Row>),
    OfferNow(Vec<Row>),
    Departing(String, GoneWhy),
    Stop,
}

/// The writer of one process's agents file. Every method only queues work for its thread, so each may be
/// called on the GTK thread; dropping it stops as [`Publisher::stop`] does.
pub struct Publisher {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Publisher {
    /// Creates `<control dir>/agents` (0700), takes the lock on `<instance>.lock` and starts the thread, which
    /// publishes the file with no rows at once. Fails, touching nothing it does not own, when the control
    /// directory's parent can be changed by another user, when the agents directory is a link or not this
    /// user's, or when the lock is held. The directory work runs on the calling thread.
    pub fn start(control_dir: &Path, instance: &str, project: ProjectRef, window: WindowRef) -> io::Result<Publisher> {
        Self::start_with(control_dir, instance, project, window, PublisherHooks::default())
    }

    #[doc(hidden)]
    pub fn start_with(
        control_dir: &Path,
        instance: &str,
        project: ProjectRef,
        window: WindowRef,
        hooks: PublisherHooks,
    ) -> io::Result<Publisher> {
        if !is_instance(instance) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{instance:?} is not an instance name (16 lowercase hex digits)"),
            ));
        }
        crate::panel_control::ensure_dir(control_dir)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
        let path = agents_dir(control_dir);
        private_fs::create_private_dir_all(&path, control_dir)?;
        let dir = open_own_dir(&path)?;
        let lock_name = OsString::from(format!("{instance}.lock"));
        let lock_path = path.join(&lock_name);
        let lock = PrivateLock::open(&lock_path)?;
        if !lock.try_lock()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} is held by another process", lock_path.display()),
            ));
        }
        let lock_identity = identity_of(&lock_path)?;
        let pid = std::process::id();
        let base = ProcessFile {
            v: SCHEMA_V,
            instance: instance.to_owned(),
            pid,
            pid_start: process_start_time(pid).unwrap_or(0),
            project,
            window,
            ..ProcessFile::default()
        };
        let writer = Writer {
            dir,
            file_name: OsString::from(format!("{instance}.json")),
            lock_name,
            lock_identity,
            lock,
            base,
            published: None,
            published_identity: None,
            departed: Vec::new(),
            marks: HashMap::new(),
            clock: hooks.clock,
            on_publish: hooks.on_publish,
            failing: false,
        };
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("eitri-agents-publish".into())
            .spawn(move || writer.run(rx))?;
        Ok(Publisher {
            tx,
            thread: Some(thread),
        })
    }

    /// Queues the rows; the thread publishes the last rows offered within `COALESCE_MS`, if they change the file.
    pub fn offer(&self, rows: Vec<Row>) {
        let _ = self.tx.send(Msg::Offer(rows));
    }

    /// Queues the rows to be published as soon as the thread reaches them, ahead of any later offer: for a tab
    /// frozen for a move, whose `moving: true` must be on disk before the hand-over.
    pub fn offer_now(&self, rows: Vec<Row>) {
        let _ = self.tx.send(Msg::OfferNow(rows));
    }

    /// Says why the row with `key` is about to leave, for the next offer that no longer holds it. A row that
    /// leaves unmarked has been closed.
    pub fn departing(&self, key: String, why: GoneWhy) {
        let _ = self.tx.send(Msg::Departing(key, why));
    }

    /// Removes the file (only if it is still the one this process published) and the lock file, releases the
    /// lock and waits for the thread.
    pub fn stop(self) {
        drop(self);
    }

    fn shut(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.shut();
    }
}

/// `path` opened as a directory without following a link, and required to be this user's with no group or
/// other permission bit: the handle every publish and removal goes through.
pub(crate) fn open_own_dir(path: &Path) -> io::Result<File> {
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("could not open {} as a directory: {e}", path.display()),
            )
        })?;
    let meta = dir.metadata()?;
    if !meta.is_dir() || meta.uid() != private_fs::euid() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of this user", path.display()),
        ));
    }
    Ok(dir)
}

fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

struct Writer {
    dir: File,
    file_name: OsString,
    lock_name: OsString,
    lock_identity: FileIdentity,
    /// Held for the life of the thread; dropped after its file is removed.
    lock: PrivateLock,
    /// Everything but the rows, the departures and the cut, which never change.
    base: ProcessFile,
    /// The last file published, and every row offered with it (some may have been cut from the file).
    published: Option<(ProcessFile, Vec<Row>)>,
    published_identity: Option<FileIdentity>,
    departed: Vec<Departed>,
    marks: HashMap<String, GoneWhy>,
    clock: fn() -> u64,
    on_publish: Option<PublishHook>,
    /// A failure is logged once, until a publish succeeds again.
    failing: bool,
}

impl Writer {
    fn run(mut self, rx: Receiver<Msg>) {
        self.publish(Vec::new());
        let coalesce = Duration::from_millis(COALESCE_MS);
        let mut pending: Option<(Vec<Row>, Instant)> = None;
        loop {
            let wait = [
                pending
                    .as_ref()
                    .map(|(_, due)| due.saturating_duration_since(Instant::now())),
                self.next_expiry(),
            ]
            .into_iter()
            .flatten()
            .min();
            let msg = match wait {
                Some(wait) => rx.recv_timeout(wait),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Msg::Offer(rows)) => {
                    let due = pending.take().map_or_else(|| Instant::now() + coalesce, |(_, due)| due);
                    pending = Some((rows, due));
                }
                Ok(Msg::OfferNow(rows)) => {
                    pending = None;
                    self.publish(rows);
                }
                Ok(Msg::Departing(key, why)) => {
                    self.marks.insert(key, why);
                }
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if pending.as_ref().is_some_and(|(_, due)| *due <= Instant::now()) {
                let (rows, _) = pending.take().expect("checked just above");
                self.publish(rows);
            }
            if self.next_expiry() == Some(Duration::ZERO) {
                let rows = self
                    .published
                    .as_ref()
                    .map(|(_, rows)| rows.clone())
                    .unwrap_or_default();
                self.publish(rows);
            }
        }
        self.remove_files();
    }

    /// How long until the oldest departure is due to leave the file, by the injected clock.
    fn next_expiry(&self) -> Option<Duration> {
        let now = (self.clock)();
        self.departed
            .iter()
            .map(|d| (d.at + DEPARTED_KEEP_MS).saturating_sub(now))
            .min()
            .map(Duration::from_millis)
    }

    /// Builds the file for `rows` and writes it when it differs from the last one published.
    fn publish(&mut self, rows: Vec<Row>) {
        let now = (self.clock)();
        let offered: HashSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        // Rows a reader was shown that are gone now. A row that was only ever cut from the file was never
        // shown, so it does not depart.
        if let Some((file, _)) = &self.published {
            for row in file.rows.iter().filter(|r| !offered.contains(r.key.as_str())) {
                if self.departed.iter().any(|d| d.key == row.key) {
                    continue;
                }
                self.departed.push(Departed {
                    id: row.id.clone(),
                    key: row.key.clone(),
                    why: self.marks.remove(&row.key).unwrap_or_default(),
                    at: now,
                });
            }
        }
        // A key that came back is not departed any more; an old departure has been seen long enough.
        self.departed
            .retain(|d| !offered.contains(d.key.as_str()) && now.saturating_sub(d.at) < DEPARTED_KEEP_MS);
        if self.departed.len() > MAX_ROWS_PER_FILE {
            let extra = self.departed.len() - MAX_ROWS_PER_FILE;
            self.departed.drain(..extra);
        }
        // A mark for a key that is neither offered nor shown has nothing left to describe.
        let shown: HashSet<&str> = self
            .published
            .as_ref()
            .map(|(file, _)| file.rows.iter().map(|r| r.key.as_str()).collect())
            .unwrap_or_default();
        self.marks
            .retain(|key, _| offered.contains(key.as_str()) || shown.contains(key.as_str()));

        let mut file = self.base.clone();
        file.rows = rows.clone();
        file.rows.sort_by_key(|r| r.number);
        if file.rows.len() > MAX_ROWS_PER_FILE {
            file.rows.truncate(MAX_ROWS_PER_FILE);
            file.truncated = true;
        }
        file.departed = self.departed.clone();
        let mut bytes = process_file_json(&file);
        // Labels are cut by characters, so 64 rows of wide text can pass the byte limit: the oldest departures
        // go first, then the highest-numbered rows.
        while bytes.len() > MAX_FILE_BYTES && !(file.rows.is_empty() && file.departed.is_empty()) {
            if file.departed.is_empty() {
                file.rows.pop();
                file.truncated = true;
            } else {
                file.departed.remove(0);
            }
            bytes = process_file_json(&file);
        }
        if self.published.as_ref().is_some_and(|(last, _)| *last == file) {
            return;
        }
        match private_fs::publish_private_file(&self.dir, &self.file_name, &bytes) {
            Ok(identity) => {
                self.failing = false;
                self.published_identity = Some(identity);
                if let Some(hook) = &mut self.on_publish {
                    hook(&file);
                }
                self.published = Some((file, rows));
            }
            Err(e) => {
                if !self.failing {
                    eprintln!("[agents] could not publish this window's rows: {e}");
                }
                self.failing = true;
            }
        }
    }

    fn remove_files(self) {
        if let Some(identity) = self.published_identity {
            if let Err(e) = private_fs::remove_if_same(&self.dir, &self.file_name, identity) {
                eprintln!("[agents] could not remove this window's rows file: {e}");
            }
        }
        // Removed while still held, so no reader takes it for a dead writer's in between.
        if let Err(e) = private_fs::remove_if_same(&self.dir, &self.lock_name, self.lock_identity) {
            eprintln!("[agents] could not remove this window's lock file: {e}");
        }
        drop(self.lock);
    }
}
