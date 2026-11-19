//! The mode keeper: the permission bits a project's files had on the server, kept when the sync rewrites them.
//!
//! Mutagen writes every file it brings from the laptop as 0600 (0700 with an executable bit), so a laptop save
//! narrows a server file's mode and a new file arrives unreadable to the group. A keeper records the mode of every
//! regular file under a registered root and puts the mode back after a write it can prove is Mutagen's:
//!
//! - a rename whose source half (`IN_MOVED_FROM`, same cookie) was seen in the tree session's staging directory beside
//!   the root (`<parent>/.mutagen-temporary-staging-<session>-beta/`, the session stages with
//!   `--stage-mode-beta=neighboring`);
//! - a rename of a `.mutagen-temporary-cross-device-rename*` file the keeper saw created and closed in the target
//!   directory during a cycle (Mutagen's copy when the root is on another file system than its staging);
//! - a file that turns up in the tree with the device, inode and birth time of a file seen in the staging directory
//!   this cycle, when the rename's destination half fell in a directory not yet watched.
//!
//! And only while the file is as Mutagen leaves it: mode exactly 0600 or 0700, one link, unchanged (inode, mode and
//! change time) between the judgement and the chmod. Every other change -- a `chmod`, a program's own write, a rename
//! inside the tree or from anywhere else -- becomes the record, so a mode somebody set on the server is never put
//! back to an older one. A Mutagen write that cannot be proved, or that was changed before its restore, is left as it
//! is, counted and named.
//!
//! A restore gives back the record's mode when the file's executability is what it was; when the laptop made it
//! executable, the record's read and write bits plus owner-execute; when the laptop made it not executable, the
//! record's read and write bits. A new file gets `0666` or `0777` (by Mutagen's executability) under the umask.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::listing::{self, c_name, open_dir_at, Reached};

/// The name Mutagen gives a neighbouring staging directory, before its session and side.
pub const STAGING_PREFIX: &str = ".mutagen-temporary-staging-";
/// The name of the copy Mutagen renames into place when the staging directory is on another file system.
pub const CROSS_DEVICE_PREFIX: &str = ".mutagen-temporary-cross-device-rename";
/// Every temporary Mutagen leaves in a tree starts so; none is a project file.
const TEMPORARY_PREFIX: &str = ".mutagen-temporary-";
/// The most ignore patterns one registration carries, and the longest one.
pub const MAX_PATTERNS: usize = 65_536;
pub const MAX_PATTERN_BYTES: usize = 4096;
/// The largest registration file; a daemon reads no more than this.
pub const MAX_REGISTRATION_BYTES: usize = 8 * 1024 * 1024;
/// How many of the paths left as they were a line names.
pub const MAX_NAMED: usize = 20;
/// How often a changed record is written out at most while the keeper runs (it is also written after the first walk
/// and when the keeper stops).
const WRITE_EVERY: Duration = Duration::from_secs(60);
/// How long a rename's source half waits for its destination half before it counts as unpaired.
const PAIR_WAIT_MS: libc::c_int = 5;
/// How many rewalks in a row a queue overflow may cause before the keeper gives up on a tree that never goes quiet.
const MAX_REWALKS_IN_A_ROW: usize = 16;
/// The most events one read pass takes in, and the most passes in one round, so a busy queue bounds memory.
const BATCH_EVENTS: usize = 4096;
const ROUND_BATCHES: usize = 64;

/// Whether a path (relative to the root, and whether it is a directory) is outside the sync, so outside the keeper.
pub type Ignore = Box<dyn Fn(&str, bool) -> bool + Send + Sync>;

/// Every name in a directory: the keeper walks the whole kept tree, bounded by its watch budget rather than a count.
fn all_names(dir: std::os::fd::RawFd) -> std::io::Result<Vec<std::ffi::OsString>> {
    listing::names_in(dir, u64::MAX).map(Option::unwrap_or_default)
}

/// The ignore rule a registration's patterns give: the one place the keeper's matcher is chosen.
pub fn ignore_for(patterns: &[String]) -> Ignore {
    listing::ignore_predicate(patterns)
}

/// What a keeper needs besides its root.
#[derive(Clone, Debug)]
pub struct Options {
    /// The account's umask, as the helper inherited it: a new file from Mutagen gets the mode it would have had.
    pub umask: u32,
    /// The server's name, for the lines a person reads.
    pub host: String,
    /// The most directories the daemon's keepers watch together; `None` for the account's inotify limit alone.
    pub watch_limit: Option<usize>,
    /// The directories the daemon's keepers watch now, shared by every keeper built from these options.
    pub watches: Arc<std::sync::atomic::AtomicUsize>,
}

/// Half of the account's inotify watch limit, so a keeper never takes every watch the account has.
pub fn watch_budget() -> Option<usize> {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .map(|limit| (limit / 2).max(1))
}

/// The server's host name, or "this server" when it has none.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid and writable for its length; gethostname writes at most that many bytes.
    let got = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(name) if got == 0 && !name.is_empty() => name.to_owned(),
        _ => "this server".to_owned(),
    }
}

/// The first 16 hex digits of the root's SHA-256: the name of its directory under `<state>/sync/`.
pub fn root_key(root: &str) -> String {
    super::hex(&Sha256::digest(root.as_bytes()).into())[..16].to_owned()
}

/// Where a root's registration, record, status and keeper lock live.
pub fn sync_dir(state: &Path, root: &str) -> PathBuf {
    state.join("sync").join(root_key(root))
}

/// The line a person reads when a root's modes are not kept.
pub fn not_kept(host: &str, root: &str, reason: &str) -> String {
    format!("file modes on {host} are not kept for {root}: {reason}")
}

/// A staging directory name the tree session can have: one name in the root's parent, Mutagen's prefix.
pub fn check_staging_name(name: &str) -> Result<(), String> {
    if name.len() > 255
        || !name.starts_with(STAGING_PREFIX)
        || name.len() == STAGING_PREFIX.len()
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(format!("{name:?} is not a Mutagen staging directory name"));
    }
    Ok(())
}

/// The mode a Mutagen write of a file gets back. `current` is the mode Mutagen wrote (0600 or 0700), `record` the
/// mode the file had. The executability Mutagen carries from the laptop decides between keeping the record,
/// adding owner-execute to its read and write bits, or keeping only its read and write bits; with no record, the
/// umask decides.
pub fn wanted(record: Option<u32>, current: u32, umask: u32) -> u32 {
    let executable = |mode: u32| mode & 0o111 != 0;
    match record {
        Some(record) => {
            let record = record & 0o777;
            match (executable(current), executable(record)) {
                (now, was) if now == was => record,
                (true, false) => (record & 0o666) | 0o100,
                _ => record & 0o666,
            }
        }
        None => (if executable(current) { 0o777 } else { 0o666 }) & !umask & 0o777,
    }
}

/// Reads a record written by a keeper: `<rel>\0<octal mode>\n` per file, sorted by path.
#[allow(dead_code)] // for tests and inspection; the keeper itself never reads its record back
pub fn read_modes_file(path: &Path) -> io::Result<BTreeMap<String, u32>> {
    let bytes = std::fs::read(path)?;
    let mut records = BTreeMap::new();
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("a record has no mode"))?;
        let rel = String::from_utf8(rest[..nul].to_vec()).map_err(io::Error::other)?;
        rest = &rest[nul + 1..];
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| io::Error::other("a record does not end"))?;
        let mode = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(|text| u32::from_str_radix(text, 8).ok())
            .ok_or_else(|| io::Error::other("a record's mode is not octal"))?;
        records.insert(rel, mode);
        rest = &rest[end + 1..];
    }
    Ok(records)
}

/// A line for the person, with the server's name and the root apart from the text (the laptop names the host as the
/// person typed it), and the paths it names. The counted line carries a `sequence` that only grows while its keeper
/// runs, and the keeper's `epoch` (new at every keeper start): every Hello carries the same line until another write
/// is counted, and the laptop shows a root's line when its (epoch, sequence) differs from the last one it showed.
/// Other lines carry sequence 0 and hold while their cause does.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModeWarning {
    pub host: String,
    pub root: String,
    pub text: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub epoch: u64,
}

/// A new keeper epoch: the start time in nanoseconds, and always above the last one this process gave out.
fn next_epoch() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let mut last = LAST.load(Ordering::SeqCst);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// The lines a keeper of `epoch` gives now.
fn keeper_warnings(status: &Status, host: &str, root: &str, epoch: u64) -> (Vec<ModeWarning>, Option<ModeWarning>) {
    let mut standing = standing_warnings(status, host, root);
    let mut counted = counted_warning(status, host, root);
    for line in standing.iter_mut().chain(counted.iter_mut()) {
        line.epoch = epoch;
    }
    (standing, counted)
}

/// What a keeper has seen so far, for the person and for tests.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Renames into the tree that could not be paired, and Mutagen writes left as they were.
    pub unpaired: u64,
    /// Of those, the ones that may have been Mutagen's (made during a sync cycle) and were left as they were.
    pub unpaired_in_sync: u64,
    /// Modes put back after a Mutagen write.
    pub restored: u64,
    /// Rewalks after the inotify queue overflowed.
    pub rewalks: u64,
    /// Why part of the tree is not watched.
    pub partial: Option<String>,
    /// Why the keeper stopped on its own.
    pub stopped: Option<String>,
    /// The last [`MAX_NAMED`] paths left as they were, each with the count (`unpaired_in_sync`) it was counted at.
    pub named: VecDeque<(u64, String)>,
}

impl Status {
    /// One more write left as it was; `in_sync` when it may have been Mutagen's, and then the person is told.
    fn left(&mut self, in_sync: bool, path: Option<&str>) {
        self.unpaired += 1;
        if !in_sync {
            return;
        }
        self.unpaired_in_sync += 1;
        if let Some(path) = path {
            self.named.retain(|(_, named)| named != path);
            self.named.push_back((self.unpaired_in_sync, path.to_owned()));
            while self.named.len() > MAX_NAMED {
                self.named.pop_front();
            }
        }
    }
}

/// The lines that hold until their cause goes: a stopped keeper, a tree watched only in part.
fn standing_warnings(status: &Status, host: &str, root: &str) -> Vec<ModeWarning> {
    let mut lines = Vec::new();
    let line = |text: String| ModeWarning {
        host: host.to_owned(),
        root: root.to_owned(),
        text,
        paths: Vec::new(),
        sequence: 0,
        epoch: 0,
    };
    if let Some(reason) = &status.stopped {
        lines.push(line(not_kept(host, root, reason)));
    }
    if let Some(reason) = &status.partial {
        lines.push(line(format!(
            "file modes on {host} are kept only for part of {root}: {reason}"
        )));
    }
    lines
}

/// The line for the writes left as they were since the keeper started, naming the last of them; its sequence is
/// their count.
fn counted_warning(status: &Status, host: &str, root: &str) -> Option<ModeWarning> {
    if status.unpaired_in_sync == 0 {
        return None;
    }
    let paths: Vec<String> = status.named.iter().map(|(_, path)| path.clone()).collect();
    let mut text = format!(
        "file modes on {host}: {} sync writes into {root} were left as Mutagen wrote them (not provably Mutagen's, or \
         changed since)",
        status.unpaired_in_sync
    );
    if !paths.is_empty() {
        text.push_str(if status.unpaired_in_sync > paths.len() as u64 {
            "; the last: "
        } else {
            ": "
        });
        text.push_str(&paths.join(", "));
    }
    Some(ModeWarning {
        host: host.to_owned(),
        root: root.to_owned(),
        text,
        paths,
        sequence: status.unpaired_in_sync,
        epoch: 0,
    })
}

/// What `<state>/sync/<16 hex>/status` holds, so a daemon that does not keep a root can pass its lines on unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusFile {
    pub warnings: Vec<ModeWarning>,
    pub counted: Option<ModeWarning>,
}

/// A running keeper of one root. Dropping it stops the keeper (and, unless discarded, writes its record out).
pub struct Keeper {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    root: String,
    host: String,
}

struct Shared {
    /// The record. The keeper holds it for each round of events, so holding it from outside pauses the keeper.
    records: Mutex<BTreeMap<String, u32>>,
    status: Mutex<Status>,
    /// The status file as last written, so it is rewritten only when it changes.
    published: Mutex<Vec<u8>>,
    wake: OwnedFd,
    stop: AtomicBool,
    /// The registration is gone: the keeper writes nothing more under its directory.
    discard: AtomicBool,
    running: AtomicBool,
    settle_asked: AtomicU64,
    settled: Mutex<u64>,
    settled_signal: Condvar,
    /// New at every keeper start, so a restarted keeper's count is not taken for an old one.
    epoch: u64,
}

impl Shared {
    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn records(&self) -> MutexGuard<'_, BTreeMap<String, u32>> {
        self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the keeper may still write under `dir`: not discarded, and its registration still there (another
    /// daemon may have removed it).
    fn registered(&self, dir: &Path) -> bool {
        !self.discard.load(Ordering::SeqCst) && dir.join("watch").exists()
    }

    fn poke(&self) {
        let one: u64 = 1;
        // SAFETY: writes eight bytes from a valid u64 to the keeper's own eventfd.
        unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    /// Writes the status file when what it would say changed.
    fn publish(&self, dir: Option<&Path>, host: &str, root: &str) {
        let Some(dir) = dir else { return };
        if !self.registered(dir) {
            return;
        }
        let mut published = self.published.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let file = {
            let status = self.status();
            let (warnings, counted) = keeper_warnings(&status, host, root, self.epoch);
            StatusFile { warnings, counted }
        };
        let Ok(bytes) = serde_json::to_vec(&file) else { return };
        if *published == bytes {
            return;
        }
        if let Ok(opened) = agent::private_fs::open_dir(dir) {
            if agent::private_fs::publish_private_file(&opened, std::ffi::OsStr::new("status"), &bytes).is_ok() {
                *published = bytes;
            }
        }
    }
}

impl Keeper {
    /// Starts keeping `root` (checked as every sync root is). `staging` names the tree session's staging directory
    /// in the root's parent; without it every Mutagen staging directory there pairs, but a staged file leaving for
    /// somewhere unwatched is not counted (it may be another root's). It refuses, with the reason, a root whose parent
    /// is not writable: Mutagen cannot stage beside it there, and its writes could not be told from anybody else's.
    /// The walk runs on the keeper's own thread; `modes_dir` receives the record and the status.
    pub fn start(
        root: &str,
        staging: Option<String>,
        ignored: Ignore,
        options: Options,
        modes_dir: Option<PathBuf>,
    ) -> Result<Keeper, String> {
        if let Some(name) = &staging {
            check_staging_name(name)?;
        }
        let held = super::root(root)?;
        let canonical = held.canonical.clone();
        let parent = Path::new(&canonical)
            .parent()
            .filter(|parent| parent != &Path::new(&canonical))
            .ok_or_else(|| "the root has no parent directory for Mutagen to stage in".to_owned())?
            .to_path_buf();
        let c_parent = c_name(parent.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
        // SAFETY: `c_parent` is NUL-terminated and outlives the call.
        if unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                c_parent.as_ptr(),
                libc::W_OK | libc::X_OK,
                libc::AT_EACCESS,
            )
        } != 0
        {
            return Err(format!(
                "{} is not writable, so Mutagen cannot stage beside the root and its writes cannot be told from others",
                parent.display()
            ));
        }
        let parent_dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent)
            .map_err(|error| format!("cannot open {}: {error}", parent.display()))?;
        engine::check_platform()?;
        let inotify = engine::new_inotify().map_err(|error| format!("cannot watch the tree: {error}"))?;
        // SAFETY: eventfd takes no pointers; a new descriptor that nothing else owns is returned.
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(format!("cannot start the keeper: {}", io::Error::last_os_error()));
        }
        let shared = Arc::new(Shared {
            records: Mutex::new(BTreeMap::new()),
            status: Mutex::new(Status::default()),
            published: Mutex::new(Vec::new()),
            // SAFETY: `wake` is a fresh descriptor owned only here.
            wake: unsafe { OwnedFd::from_raw_fd(wake) },
            stop: AtomicBool::new(false),
            discard: AtomicBool::new(false),
            running: AtomicBool::new(true),
            settle_asked: AtomicU64::new(0),
            settled: Mutex::new(0),
            settled_signal: Condvar::new(),
            epoch: next_epoch(),
        });
        let host = options.host.clone();
        let engine = engine::Engine::new(engine::Setup {
            shared: shared.clone(),
            root: held.dir,
            root_text: canonical.clone(),
            parent: parent_dir,
            inotify,
            staging: staging.map(String::into_bytes),
            ignored,
            options,
            modes_dir: modes_dir.clone(),
        });
        let thread = std::thread::Builder::new()
            .name("eitri-modes".to_owned())
            .spawn(move || engine.run())
            .map_err(|error| format!("cannot start the keeper: {error}"))?;
        Ok(Keeper {
            shared,
            thread: Some(thread),
            root: canonical,
            host,
        })
    }

    /// The root as it resolved.
    #[allow(dead_code)]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Whether the keeper still runs (it stops on its own when its root is moved or removed).
    pub fn running(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    /// Returns once every change made before the call has been looked at and acted on, and the record written out.
    #[allow(dead_code)] // for tests: the daemon never waits for a keeper
    pub fn settle(&self) {
        let asked = self.shared.settle_asked.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.poke();
        let mut settled = self
            .shared
            .settled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *settled < asked && self.running() {
            settled = self
                .shared
                .settled_signal
                .wait_timeout(settled, Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Pauses the keeper until the guard is dropped: changes meanwhile wait in the inotify queue.
    #[allow(dead_code)] // for tests
    pub fn hold(&self) -> MutexGuard<'_, BTreeMap<String, u32>> {
        self.shared.records()
    }

    /// The recorded mode of `rel`.
    #[allow(dead_code)] // for tests
    pub fn recorded(&self, rel: &str) -> Option<u32> {
        self.shared.records().get(rel).copied()
    }

    pub fn status(&self) -> Status {
        self.shared.status().clone()
    }

    /// What a person should know about this keeper now. Asking changes nothing: every caller gets the same lines.
    pub fn warnings(&self) -> Vec<ModeWarning> {
        let status = self.status();
        let (mut lines, counted) = keeper_warnings(&status, &self.host, &self.root, self.shared.epoch);
        lines.extend(counted);
        lines
    }

    /// The registration is gone: the keeper writes nothing more under its directory.
    fn discard(&self) {
        self.shared.discard.store(true, Ordering::SeqCst);
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.poke();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the keeper knows about a file: its identity, mode, links and times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Facts {
    dev: u64,
    ino: u64,
    /// The whole `st_mode`, type included.
    mode: u32,
    nlink: u32,
    ctime: (i64, u32),
    /// The birth time, when the file system reports one.
    btime: Option<(i64, u32)>,
}

impl Facts {
    fn regular(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    fn directory(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    /// Whether the file is as Mutagen leaves it: one link, mode 0600 or 0700 exactly. Anything else means somebody
    /// changed it since.
    fn as_mutagen_writes(&self) -> bool {
        self.regular() && self.nlink == 1 && matches!(self.mode & 0o7777, 0o600 | 0o700)
    }

    /// The staged file's identity: device, inode and birth time (no birth time, no identity).
    fn identity(&self) -> Option<Identity> {
        Some((self.dev, self.ino, self.btime?))
    }
}

/// A staged file as it keeps through a rename: device, inode and birth time.
type Identity = (u64, u64, (i64, u32));

/// The identity of `facts` (a file found in the tree) when it is one seen in staging this cycle and the file is still
/// as Mutagen leaves it; a file without a birth time, or with another one, is no match.
fn staged_match(staged: &HashMap<Identity, u32>, facts: &Facts) -> Option<Identity> {
    let identity = facts.identity()?;
    (facts.as_mutagen_writes() && staged.contains_key(&identity)).then_some(identity)
}

fn statx(dir: RawFd, name: &CStr, flags: libc::c_int) -> Option<Facts> {
    // SAFETY: an all-zero `statx` is a valid value for the call to overwrite.
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `name` is NUL-terminated and outlives the call, `st` is valid and writable.
        let got = unsafe {
            libc::statx(
                dir,
                name.as_ptr(),
                flags | libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_SYNC_AS_STAT,
                libc::STATX_BASIC_STATS | libc::STATX_BTIME,
                &mut st,
            )
        };
        if got == 0 {
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
    let btime = (st.stx_mask & libc::STATX_BTIME != 0).then_some((st.stx_btime.tv_sec, st.stx_btime.tv_nsec));
    Some(Facts {
        dev: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
        ino: st.stx_ino,
        mode: st.stx_mode as u32,
        nlink: st.stx_nlink,
        ctime: (st.stx_ctime.tv_sec, st.stx_ctime.tv_nsec),
        btime,
    })
}

/// What `name` in `dir` is, a link not followed.
fn facts_at(dir: RawFd, name: &CStr) -> Option<Facts> {
    statx(dir, name, 0)
}

/// What the descriptor holds.
fn facts_fd(fd: RawFd) -> Option<Facts> {
    statx(fd, c"", libc::AT_EMPTY_PATH)
}

/// Why a directory got no watch: the keeper's budget or the account's limit (shown to the person).
#[derive(Debug)]
struct NoWatch(String);

impl std::fmt::Display for NoWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoWatch {}

/// One planned restore: the file as it was when the write was judged Mutagen's, and the mode it gets.
#[derive(Clone, Copy, Debug)]
struct Restore {
    facts: Facts,
    want: u32,
}

/// Where a rename's source half was seen.
#[derive(Clone, Debug)]
enum From {
    /// In a staging directory; `landing` when it was a staged file leaving a `<2 hex>` subdirectory (not a move
    /// inside the staging directory), with its identity when the keeper saw one.
    Staging { landing: bool, identity: Option<Identity> },
    /// A staged file already placed in the tree by its identity.
    Placed,
    /// Mutagen's cross-device copy, seen created and closed in the target directory during a cycle.
    CrossDevice,
    /// A path in the tree.
    Tree { rel: String, dir: bool },
    /// A path the keeper does not keep (ignored, or a Mutagen temporary it did not see made).
    Ignored,
}

#[derive(Clone)]
enum Watch {
    /// The root's parent, for the staging directory.
    Parent,
    /// A directory of a staging tree, by its path from the root's parent (`<staging>` or `<staging>/<2 hex>`).
    Staging { path: Vec<u8> },
    /// A directory of the tree, by its path from the root.
    Tree { rel: String },
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The keys below the directory `rel`: in byte order they sit between `rel/` and `rel0` ('0' follows '/').
#[cfg(target_os = "linux")]
fn inside<V>(map: &BTreeMap<String, V>, rel: &str) -> Vec<String> {
    map.range(format!("{rel}/")..format!("{rel}0"))
        .map(|(path, _)| path.clone())
        .collect()
}

#[cfg(target_os = "linux")]
fn under(path: &str, prefix: &str) -> bool {
    path == prefix || (path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/')
}

#[cfg(not(target_os = "linux"))]
mod engine {
    use super::*;

    pub fn check_platform() -> Result<(), String> {
        Err("keeping file modes needs inotify, which only Linux has".to_owned())
    }

    pub fn new_inotify() -> io::Result<OwnedFd> {
        Err(io::Error::other("inotify is Linux-only"))
    }

    pub struct Setup {
        pub shared: Arc<Shared>,
        pub root: File,
        pub root_text: String,
        pub parent: File,
        pub inotify: OwnedFd,
        pub staging: Option<Vec<u8>>,
        pub ignored: Ignore,
        pub options: Options,
        pub modes_dir: Option<PathBuf>,
    }

    pub struct Engine;

    impl Engine {
        pub fn new(_: Setup) -> Engine {
            Engine
        }

        pub fn run(self) {}
    }
}

#[cfg(target_os = "linux")]
mod engine {
    use super::*;

    const TREE_EVENTS: u32 = libc::IN_ATTRIB
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MODIFY
        | libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_EXCL_UNLINK
        | libc::IN_ONLYDIR;
    const STAGING_EVENTS: u32 =
        libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_ONLYDIR;
    const PARENT_EVENTS: u32 =
        libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_ONLYDIR;
    /// Deeper than Mutagen's staging layout (`<staging>/<2 hex>/<digest>`) ever goes.
    const MAX_STAGING_DEPTH: usize = 4;

    pub fn check_platform() -> Result<(), String> {
        Ok(())
    }

    pub fn new_inotify() -> io::Result<OwnedFd> {
        // SAFETY: inotify_init1 takes no pointers; a new descriptor that nothing else owns is returned.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor owned only here.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    struct Event {
        wd: i32,
        mask: u32,
        cookie: u32,
        name: Vec<u8>,
    }

    pub struct Setup {
        pub shared: Arc<Shared>,
        pub root: File,
        pub root_text: String,
        pub parent: File,
        pub inotify: OwnedFd,
        pub staging: Option<Vec<u8>>,
        pub ignored: Ignore,
        pub options: Options,
        pub modes_dir: Option<PathBuf>,
    }

    pub struct Engine {
        shared: Arc<Shared>,
        root: File,
        root_text: String,
        parent: File,
        inotify: OwnedFd,
        /// The tree session's staging directory name, when the registration names it.
        staging: Option<Vec<u8>>,
        ignored: Ignore,
        options: Options,
        modes_dir: Option<PathBuf>,
        watches: HashMap<i32, Watch>,
        tree_watches: usize,
        /// This root's staging directories in the parent, by the parent's own events (whether or not a watch on them
        /// took): Mutagen is in a cycle while one exists.
        staging_names: HashSet<Vec<u8>>,
        /// Source halves of renames still waiting for their destination half, by cookie.
        from: HashMap<u32, From>,
        /// The files seen in a staging directory during the current cycle, by watch and name, and by identity. A file
        /// that later turns up in the tree with one of these identities came out of the staging directory, even when
        /// the rename's destination half fell in a directory not yet watched. An identity goes when its staging file
        /// is deleted or leaves the staging directory without a paired tree destination.
        staged: HashMap<(i32, Vec<u8>), Identity>,
        /// How many staging names hold each identity (a file seen again under a new name before its old name went).
        staged_ids: HashMap<Identity, u32>,
        /// Staged files already placed by identity, whose source half may still be on its way.
        placed: HashSet<Identity>,
        /// Cross-device copies seen created in a tree directory during a cycle, by watch and name; `true` once closed
        /// after writing.
        copies: HashMap<(i32, Vec<u8>), bool>,
        /// Restores planned, by path; any later change to the path cancels its plan. They run once the queue is
        /// drained.
        restores: BTreeMap<String, Restore>,
        /// Set while events from before a walk's end are drained: they update the record and restore nothing.
        quiet: bool,
        overflowed: bool,
        /// Whether the last round read the queue to its end.
        drained: bool,
        dirty: bool,
        written: Instant,
    }

    impl Drop for Engine {
        fn drop(&mut self) {
            self.release_tree_watches(self.tree_watches);
        }
    }

    impl Engine {
        pub fn new(setup: Setup) -> Engine {
            Engine {
                shared: setup.shared,
                root: setup.root,
                root_text: setup.root_text,
                parent: setup.parent,
                inotify: setup.inotify,
                staging: setup.staging,
                ignored: setup.ignored,
                options: setup.options,
                modes_dir: setup.modes_dir,
                watches: HashMap::new(),
                tree_watches: 0,
                staging_names: HashSet::new(),
                from: HashMap::new(),
                staged: HashMap::new(),
                staged_ids: HashMap::new(),
                placed: HashSet::new(),
                copies: HashMap::new(),
                restores: BTreeMap::new(),
                quiet: false,
                overflowed: false,
                drained: true,
                dirty: false,
                written: Instant::now(),
            }
        }

        fn publish(&self) {
            self.shared
                .publish(self.modes_dir.as_deref(), &self.options.host, &self.root_text);
        }

        fn left(&self, in_sync: bool, path: Option<&str>) {
            self.shared.status().left(in_sync, path);
        }

        pub fn run(mut self) {
            let shared = self.shared.clone();
            let started = {
                let mut records = shared.records();
                self.walk_afresh(&mut records, false)
            };
            if let Err(reason) = started {
                self.stop_with(reason);
                return;
            }
            self.write_out(&shared.records());
            self.publish();
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let timeout = if !self.drained {
                    0
                } else if self.dirty {
                    WRITE_EVERY.saturating_sub(self.written.elapsed()).as_millis() as libc::c_int
                } else {
                    -1
                };
                let mut fds = [
                    libc::pollfd {
                        fd: self.inotify.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: shared.wake.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: `fds` is valid and writable for its two entries.
                let polled = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
                if polled < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    self.stop_with(format!("the watch failed: {}", io::Error::last_os_error()));
                    return;
                }
                let mut count = [0u8; 8];
                // SAFETY: reads at most eight bytes into `count`; the eventfd is non-blocking.
                unsafe { libc::read(shared.wake.as_raw_fd(), count.as_mut_ptr().cast(), 8) };
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let asked = shared.settle_asked.load(Ordering::SeqCst);
                let settling = asked > *shared.settled.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let outcome = {
                    let mut records = shared.records();
                    let mut outcome = self.round(&mut records);
                    // A settle waits for the whole queue, however long it is.
                    while settling && outcome.is_ok() && !self.drained && !shared.stop.load(Ordering::SeqCst) {
                        outcome = self.round(&mut records);
                    }
                    if outcome.is_ok()
                        && self.dirty
                        && self.drained
                        && (settling || self.written.elapsed() >= WRITE_EVERY)
                    {
                        self.write_out(&records);
                    }
                    outcome
                };
                if let Err(reason) = outcome {
                    self.stop_with(reason);
                    return;
                }
                self.publish();
                if self.drained {
                    let mut settled = shared.settled.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if asked > *settled {
                        *settled = asked;
                    }
                    shared.settled_signal.notify_all();
                }
            }
            let records = shared.records();
            if self.dirty {
                self.write_out(&records);
            }
            drop(records);
            shared.running.store(false, Ordering::SeqCst);
            shared.settled_signal.notify_all();
        }

        fn stop_with(&mut self, reason: String) {
            self.shared.status().stopped = Some(reason);
            let shared = self.shared.clone();
            let records = shared.records();
            if self.dirty {
                self.write_out(&records);
            }
            drop(records);
            self.publish();
            self.shared.running.store(false, Ordering::SeqCst);
            self.shared.settled_signal.notify_all();
        }

        /// Starts over on a new inotify instance: the parent and this root's staging directory watched, the tree
        /// walked with a watch on every directory before it is read, and the record replaced by the modes found.
        /// Changes made during the walk are then drained into the record without restoring anything.
        fn walk_afresh(&mut self, records: &mut BTreeMap<String, u32>, after_overflow: bool) -> Result<(), String> {
            for _ in 0..MAX_REWALKS_IN_A_ROW {
                if after_overflow || !self.watches.is_empty() {
                    self.inotify = new_inotify().map_err(|error| format!("cannot watch the tree: {error}"))?;
                }
                self.watches.clear();
                self.release_tree_watches(self.tree_watches);
                self.staging_names.clear();
                self.from.clear();
                self.forget_staged();
                self.copies.clear();
                self.restores.clear();
                self.overflowed = false;
                self.shared.status().partial = None;
                let parent = self.parent_fd()?;
                self.add_watch(&parent, PARENT_EVENTS, Watch::Parent)
                    .map_err(|error| format!("cannot watch the root's parent: {error}"))?;
                self.scan_for_staging(&parent);
                records.clear();
                let root = self
                    .root
                    .try_clone()
                    .map(OwnedFd::from)
                    .map_err(|error| format!("cannot walk the root: {error}"))?;
                self.walk_dir(records, root, "", 0, false)?;
                self.dirty = true;
                self.quiet = true;
                let drained = loop {
                    if let Err(error) = self.round(records) {
                        break Err(error);
                    }
                    if self.overflowed || self.drained || self.shared.stop.load(Ordering::SeqCst) {
                        break Ok(());
                    }
                };
                self.quiet = false;
                drained?;
                if !self.overflowed {
                    return Ok(());
                }
                self.shared.status().rewalks += 1;
            }
            Err("the tree changes faster than it can be walked".to_owned())
        }

        fn parent_fd(&self) -> Result<OwnedFd, String> {
            self.parent
                .try_clone()
                .map(OwnedFd::from)
                .map_err(|error| format!("cannot watch the root's parent: {error}"))
        }

        fn add_watch(&mut self, dir: &OwnedFd, mask: u32, watch: Watch) -> io::Result<i32> {
            let is_tree = matches!(watch, Watch::Tree { .. });
            if is_tree {
                // One watch of the daemon's budget is taken up front, and given back below when none was added.
                let used = self.options.watches.fetch_add(1, Ordering::SeqCst);
                if let Some(limit) = self.options.watch_limit {
                    if used >= limit {
                        self.options.watches.fetch_sub(1, Ordering::SeqCst);
                        return Err(io::Error::other(NoWatch(format!(
                            "the keepers' watch budget ({limit} directories, half of fs.inotify.max_user_watches) is \
                             reached"
                        ))));
                    }
                }
            }
            let added = self.add_inotify_watch(dir, mask, watch);
            if is_tree && !matches!(added, Ok((_, true))) {
                self.options.watches.fetch_sub(1, Ordering::SeqCst);
            }
            added.map(|(wd, _)| wd)
        }

        /// Adds the watch; the flag says whether it is a new tree watch (not one already held for that directory).
        fn add_inotify_watch(&mut self, dir: &OwnedFd, mask: u32, watch: Watch) -> io::Result<(i32, bool)> {
            // The descriptor's own entry under /proc names the very directory it holds, wherever that is now.
            let path = c_name(format!("/proc/self/fd/{}", dir.as_raw_fd()).as_bytes())?;
            // SAFETY: `path` is NUL-terminated and outlives the call; the inotify descriptor is open.
            let wd = unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), path.as_ptr(), mask) };
            if wd < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOSPC) {
                    return Err(io::Error::other(NoWatch(
                        "the account's inotify watch limit (fs.inotify.max_user_watches) is reached".to_owned(),
                    )));
                }
                return Err(error);
            }
            let is_tree = matches!(watch, Watch::Tree { .. });
            let new_tree = match self.watches.insert(wd, watch) {
                Some(Watch::Tree { .. }) if is_tree => false,
                Some(Watch::Tree { .. }) => {
                    self.release_tree_watches(1);
                    false
                }
                _ => is_tree,
            };
            if new_tree {
                self.tree_watches += 1;
            }
            Ok((wd, new_tree))
        }

        fn unwatch(&mut self, wd: i32) {
            if let Some(Watch::Tree { .. }) = self.watches.remove(&wd) {
                self.release_tree_watches(1);
            }
        }

        /// Gives `count` tree watches back to the daemon's budget.
        fn release_tree_watches(&mut self, count: usize) {
            let count = count.min(self.tree_watches);
            self.tree_watches -= count;
            self.options.watches.fetch_sub(count, Ordering::SeqCst);
        }

        fn watch_failed(&mut self, error: &io::Error) {
            if let Some(reason) = error.get_ref().and_then(|inner| inner.downcast_ref::<NoWatch>()) {
                self.shared.status().partial =
                    Some(format!("{reason}; files elsewhere keep the mode Mutagen gives them"));
            }
        }

        fn forget_staged(&mut self) {
            self.staged.clear();
            self.staged_ids.clear();
            self.placed.clear();
        }

        /// Whether `name` in the parent is a staging directory of this root's.
        fn ours(&self, name: &[u8]) -> bool {
            match &self.staging {
                Some(staging) => name == staging.as_slice(),
                None => name.starts_with(STAGING_PREFIX.as_bytes()),
            }
        }

        fn in_cycle(&self) -> bool {
            !self.staging_names.is_empty()
        }

        /// Opens the staging directory at `path` (from the root's parent), one name at a time, never through a link.
        fn open_staging(&self, path: &[u8]) -> Option<OwnedFd> {
            let mut dir = self.parent_fd().ok()?;
            for part in path.split(|b| *b == b'/') {
                dir = open_dir_at(dir.as_raw_fd(), &c_name(part).ok()?).ok()?;
            }
            Some(dir)
        }

        /// Watches every staging directory of this root's in the parent (a cycle may be running).
        fn scan_for_staging(&mut self, parent: &OwnedFd) {
            let Ok(names) = all_names(parent.as_raw_fd()) else {
                return;
            };
            for name in names {
                let name = name.as_bytes().to_vec();
                let Ok(c_leaf) = c_name(&name) else { continue };
                if self.ours(&name) && facts_at(parent.as_raw_fd(), &c_leaf).is_some_and(|f| f.directory()) {
                    self.staging_names.insert(name.clone());
                    self.watch_staging(name, 0);
                }
            }
        }

        /// Watches the staging directory at `path`, then every directory and file already in it: one made before its
        /// watch existed sends nothing.
        fn watch_staging(&mut self, path: Vec<u8>, depth: usize) {
            if depth > MAX_STAGING_DEPTH {
                return;
            }
            let Some(dir) = self.open_staging(&path) else { return };
            let wd = match self.add_watch(&dir, STAGING_EVENTS, Watch::Staging { path: path.clone() }) {
                Ok(wd) => wd,
                Err(error) => {
                    self.watch_failed(&error);
                    return;
                }
            };
            let Ok(names) = all_names(dir.as_raw_fd()) else {
                return;
            };
            for name in names {
                let Ok(c_leaf) = c_name(name.as_bytes()) else { continue };
                let Some(facts) = facts_at(dir.as_raw_fd(), &c_leaf) else {
                    continue;
                };
                if facts.directory() {
                    let mut sub = path.clone();
                    sub.push(b'/');
                    sub.extend_from_slice(name.as_bytes());
                    self.watch_staging(sub, depth + 1);
                } else if let Some(identity) = facts.regular().then(|| facts.identity()).flatten() {
                    self.note_staged(wd, name.as_bytes(), identity);
                }
            }
        }

        fn note_staged(&mut self, wd: i32, name: &[u8], identity: Identity) {
            if let Some(old) = self.staged.insert((wd, name.to_vec()), identity) {
                self.unname(old);
            }
            *self.staged_ids.entry(identity).or_insert(0) += 1;
        }

        /// The staging name `key` is gone; its identity, if it had one.
        fn unstage(&mut self, key: &(i32, Vec<u8>)) -> Option<Identity> {
            let identity = self.staged.remove(key)?;
            self.unname(identity);
            Some(identity)
        }

        fn unname(&mut self, identity: Identity) {
            if let Some(count) = self.staged_ids.get_mut(&identity) {
                *count = count.saturating_sub(1);
            }
        }

        /// A staged identity left its staging name: it is no longer one, unless the file was seen again under
        /// another name in the staging directory (a scan that came after the move).
        fn drop_staged(&mut self, identity: Option<Identity>) {
            if let Some(identity) = identity {
                if self.staged_ids.get(&identity) == Some(&0) {
                    self.staged_ids.remove(&identity);
                }
            }
        }

        /// Whether `facts` (a file just found in the tree) is a file seen in staging this cycle and still as Mutagen
        /// left it; if so it is taken as placed.
        fn take_staged(&mut self, facts: &Facts) -> bool {
            match staged_match(&self.staged_ids, facts) {
                Some(identity) => {
                    self.staged_ids.remove(&identity);
                    self.placed.insert(identity);
                    true
                }
                None => false,
            }
        }

        fn skipped(&self, rel: &str, name: &str, is_dir: bool) -> bool {
            name.eq_ignore_ascii_case(".git") || name.starts_with(TEMPORARY_PREFIX) || (self.ignored)(rel, is_dir)
        }

        /// Watches the tree directory `dir` (at `rel`), then records every file in it and walks into every directory:
        /// the watch comes first, so nothing made while the directory is read goes unseen. For a directory that
        /// `arrived` while the keeper ran, a file found there that was seen in staging this cycle is a Mutagen write
        /// that landed before the watch existed, and is treated as one; one that only looks like Mutagen's, made
        /// during a cycle, is left as it is and counted.
        fn walk_dir(
            &mut self,
            records: &mut BTreeMap<String, u32>,
            dir: OwnedFd,
            rel: &str,
            depth: usize,
            arrived: bool,
        ) -> Result<(), String> {
            if self.shared.stop.load(Ordering::SeqCst) {
                return Err("the keeper is stopping".to_owned());
            }
            if depth > listing::MAX_DEPTH {
                return Ok(());
            }
            if let Err(error) = self.add_watch(&dir, TREE_EVENTS, Watch::Tree { rel: rel.to_owned() }) {
                if depth == 0 {
                    return Err(format!("cannot watch the root: {error}"));
                }
                self.watch_failed(&error);
            }
            let names = match all_names(dir.as_raw_fd()) {
                Ok(names) => names,
                Err(_) if depth > 0 => return Ok(()),
                Err(error) => return Err(format!("cannot read the root: {error}")),
            };
            for name in names {
                let Some(text) = name.to_str() else { continue };
                let Ok(c_leaf) = c_name(name.as_bytes()) else { continue };
                let Some(facts) = facts_at(dir.as_raw_fd(), &c_leaf) else {
                    continue;
                };
                let child = join(rel, text);
                if self.skipped(&child, text, facts.directory()) {
                    continue;
                }
                if facts.directory() {
                    if let Ok(sub) = open_dir_at(dir.as_raw_fd(), &c_leaf) {
                        self.walk_dir(records, sub, &child, depth + 1, arrived)?;
                    }
                } else if facts.regular() {
                    self.dirty = true;
                    let live = arrived && !self.quiet;
                    if live && self.take_staged(&facts) {
                        records.remove(&child);
                        self.mutagen_write(records, &child);
                        continue;
                    }
                    if live && self.in_cycle() && facts.mode & 0o077 == 0 {
                        self.left(true, Some(&child));
                    }
                    records.insert(child, facts.mode & 0o777);
                }
            }
            Ok(())
        }

        /// Reads the events queued now, at most [`BATCH_EVENTS`], without waiting; stops at a queue overflow. The flag
        /// says whether the queue was read to its end.
        fn drain(&mut self) -> Result<(Vec<Event>, bool), String> {
            let mut events = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            while events.len() < BATCH_EVENTS {
                // SAFETY: `buffer` is valid and writable for its length; the descriptor is open and non-blocking.
                let read = unsafe { libc::read(self.inotify.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if read < 0 {
                    let error = io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EAGAIN) => return Ok((events, true)),
                        Some(libc::EINTR) => continue,
                        _ => return Err(format!("the watch failed: {error}")),
                    }
                }
                let read = read as usize;
                let header = std::mem::size_of::<libc::inotify_event>();
                let mut offset = 0;
                while offset + header <= read {
                    // SAFETY: the kernel wrote whole events; the header is read unaligned from inside the buffer.
                    let event: libc::inotify_event =
                        unsafe { std::ptr::read_unaligned(buffer[offset..].as_ptr().cast()) };
                    let name_start = offset + header;
                    let name_end = (name_start + event.len as usize).min(read);
                    let name = &buffer[name_start..name_end];
                    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                    if event.mask & libc::IN_Q_OVERFLOW != 0 {
                        self.overflowed = true;
                        return Ok((events, false));
                    }
                    events.push(Event {
                        wd: event.wd,
                        mask: event.mask,
                        cookie: event.cookie,
                        name: name.to_vec(),
                    });
                    offset = name_end;
                }
            }
            Ok((events, false))
        }

        fn readable_within(&self, milliseconds: libc::c_int) -> bool {
            let mut fd = libc::pollfd {
                fd: self.inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `fd` is valid and writable.
            unsafe { libc::poll(&mut fd, 1, milliseconds) > 0 }
        }

        /// Takes in what is queued, in bounded passes (and what follows within a few milliseconds while a rename's
        /// halves are apart). Once the queue is read to its end it acts: renames still unpaired are settled and the
        /// planned restores run. A queue overflow cancels every plan, and the tree is walked afresh.
        fn round(&mut self, records: &mut BTreeMap<String, u32>) -> Result<(), String> {
            let mut waited = false;
            self.drained = false;
            for _ in 0..ROUND_BATCHES {
                if self.shared.stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let (events, at_end) = self.drain()?;
                for event in &events {
                    self.handle(records, event)?;
                }
                if self.overflowed {
                    break;
                }
                if at_end && events.is_empty() {
                    if !self.from.is_empty() && !waited {
                        waited = true;
                        if self.readable_within(PAIR_WAIT_MS) {
                            continue;
                        }
                    }
                    self.drained = true;
                    break;
                }
                if !events.is_empty() {
                    waited = false;
                }
            }
            if self.overflowed {
                self.restores.clear();
                self.drained = true;
                if self.quiet {
                    // The walk that is draining starts over.
                    return Ok(());
                }
                self.shared.status().rewalks += 1;
                return self.walk_afresh(records, true);
            }
            if self.drained {
                self.settle_unpaired(records);
                self.apply_restores(records);
            }
            Ok(())
        }

        fn handle(&mut self, records: &mut BTreeMap<String, u32>, event: &Event) -> Result<(), String> {
            let Some(watch) = self.watches.get(&event.wd).cloned() else {
                return Ok(());
            };
            if event.mask & libc::IN_IGNORED != 0 {
                self.unwatch(event.wd);
                return Ok(());
            }
            let is_dir = event.mask & libc::IN_ISDIR != 0;
            let moved_to = event.mask & libc::IN_MOVED_TO != 0;
            match watch {
                Watch::Parent => {
                    if moved_to {
                        self.landed_outside(records, event.cookie);
                    }
                    if !(is_dir && self.ours(&event.name)) {
                        return Ok(());
                    }
                    if event.mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0 {
                        self.staging_names.remove(&event.name);
                        if !self.in_cycle() {
                            // No cycle runs: what was staged has landed or is gone, and its inodes may be used again.
                            self.forget_staged();
                            self.copies.clear();
                        }
                    }
                    if event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                        self.staging_names.insert(event.name.clone());
                        self.watch_staging(event.name.clone(), 0);
                    }
                }
                Watch::Staging { path } => self.staging_event(records, &path, event, is_dir),
                Watch::Tree { rel: dir_rel } => self.tree_event(records, &dir_rel, event, is_dir)?,
            }
            Ok(())
        }

        fn staging_event(&mut self, records: &mut BTreeMap<String, u32>, path: &[u8], event: &Event, is_dir: bool) {
            let key = (event.wd, event.name.clone());
            let landing = path.contains(&b'/');
            if event.mask & libc::IN_MOVED_FROM != 0 && !is_dir {
                let identity = self.unstage(&key);
                let from = if identity.is_some_and(|identity| self.placed.remove(&identity)) {
                    From::Placed
                } else {
                    From::Staging { landing, identity }
                };
                self.from.insert(event.cookie, from);
            }
            if event.mask & libc::IN_DELETE != 0 && !is_dir {
                let identity = self.unstage(&key);
                self.drop_staged(identity);
            }
            if event.mask & libc::IN_MOVED_TO != 0 {
                match self.from.remove(&event.cookie) {
                    // A move inside the staging directory: the file is still staged, under its new name.
                    Some(From::Staging {
                        identity: Some(identity),
                        ..
                    }) if !is_dir => self.note_staged(event.wd, &event.name, identity),
                    Some(From::Tree { rel, dir }) => self.left_tree(records, &rel, dir),
                    _ => {}
                }
            } else if event.mask & libc::IN_CREATE != 0 && !is_dir {
                let identity = self
                    .open_staging(path)
                    .and_then(|dir| facts_at(dir.as_raw_fd(), &c_name(&event.name).ok()?))
                    .filter(Facts::regular)
                    .and_then(|facts| facts.identity());
                if let Some(identity) = identity {
                    self.note_staged(event.wd, &event.name, identity);
                }
            }
            if is_dir && event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                let mut sub = path.to_vec();
                sub.push(b'/');
                sub.extend_from_slice(&event.name);
                self.watch_staging(sub, path.split(|b| *b == b'/').count());
            }
        }

        /// A rename's destination half outside the tree: a tree path that went there has left the tree, and a staged
        /// identity that went there is no longer one.
        fn landed_outside(&mut self, records: &mut BTreeMap<String, u32>, cookie: u32) {
            match self.from.remove(&cookie) {
                Some(From::Tree { rel, dir }) => self.left_tree(records, &rel, dir),
                Some(From::Staging { identity, .. }) => self.drop_staged(identity),
                _ => {}
            }
        }

        /// A planned restore is off; unless the file itself is gone, it is a Mutagen write left as it is.
        fn cancel_restore(&mut self, rel: &str, count: bool) {
            if self.restores.remove(rel).is_some() && count {
                self.left(true, Some(rel));
            }
        }

        fn tree_event(
            &mut self,
            records: &mut BTreeMap<String, u32>,
            dir_rel: &str,
            event: &Event,
            is_dir: bool,
        ) -> Result<(), String> {
            if event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                if dir_rel.is_empty() {
                    return Err("the root was moved or removed".to_owned());
                }
                return Ok(());
            }
            // A name that is not UTF-8 cannot be synced, so Mutagen never writes it.
            let Ok(name) = std::str::from_utf8(&event.name) else {
                return Ok(());
            };
            if name.is_empty() {
                return Ok(());
            }
            let rel = join(dir_rel, name);
            let mask = event.mask;
            if self.skipped(&rel, name, is_dir) {
                self.temporary_event(records, &rel, name, event, is_dir);
                return Ok(());
            }
            if mask & libc::IN_MOVED_TO != 0 {
                let from = self.from.remove(&event.cookie);
                if matches!(from, Some(From::Placed)) && !is_dir {
                    // The file was already found here by its identity, and its restore is planned.
                    return Ok(());
                }
                self.cancel_restore(&rel, false);
                self.moved_in(records, &rel, from, is_dir);
                return Ok(());
            }
            if mask & (libc::IN_MODIFY | libc::IN_CLOSE_WRITE) != 0 {
                // Somebody wrote the file after Mutagen did: its mode is theirs now.
                if self.restores.contains_key(&rel) {
                    self.cancel_restore(&rel, true);
                    self.adopt(records, &rel);
                }
                return Ok(());
            }
            if mask & libc::IN_CREATE != 0 {
                self.cancel_restore(&rel, false);
                if is_dir {
                    self.watch_tree_rel(records, &rel, true);
                } else {
                    self.adopt(records, &rel);
                }
            } else if mask & libc::IN_ATTRIB != 0 {
                if !is_dir {
                    self.cancel_restore(&rel, true);
                    self.adopt(records, &rel);
                }
            } else if mask & libc::IN_DELETE != 0 {
                self.cancel_restore(&rel, false);
                self.forget(records, &rel);
            } else if mask & libc::IN_MOVED_FROM != 0 {
                self.cancel_restore(&rel, true);
                self.from.insert(
                    event.cookie,
                    From::Tree {
                        rel: rel.clone(),
                        dir: is_dir,
                    },
                );
            }
            Ok(())
        }

        /// Events on a Mutagen temporary or an ignored path: none is recorded; a cross-device copy is followed from its
        /// creation, so only one Mutagen made can vouch for the rename out of it.
        fn temporary_event(
            &mut self,
            records: &mut BTreeMap<String, u32>,
            rel: &str,
            name: &str,
            event: &Event,
            is_dir: bool,
        ) {
            let key = (event.wd, event.name.clone());
            let copy = !is_dir && name.starts_with(CROSS_DEVICE_PREFIX) && !(self.ignored)(rel, false);
            let mask = event.mask;
            if copy && mask & libc::IN_CREATE != 0 && self.in_cycle() {
                self.copies.insert(key.clone(), false);
            }
            if copy && mask & libc::IN_CLOSE_WRITE != 0 {
                if let Some(closed) = self.copies.get_mut(&key) {
                    *closed = true;
                }
            }
            if mask & libc::IN_DELETE != 0 {
                self.copies.remove(&key);
            }
            if mask & libc::IN_MOVED_FROM != 0 {
                let from = if copy && self.copies.remove(&key) == Some(true) {
                    From::CrossDevice
                } else {
                    From::Ignored
                };
                self.from.insert(event.cookie, from);
            } else if mask & libc::IN_MOVED_TO != 0 {
                // A temporary name reached by a rename is nobody's copy.
                self.copies.remove(&key);
                self.landed_outside(records, event.cookie);
            }
        }

        /// The destination half of a rename into the kept tree.
        fn moved_in(&mut self, records: &mut BTreeMap<String, u32>, rel: &str, from: Option<From>, is_dir: bool) {
            match from {
                Some(From::Staging { identity, .. }) if !is_dir => {
                    self.drop_staged(identity);
                    self.mutagen_write(records, rel);
                }
                Some(From::CrossDevice) if !is_dir => self.mutagen_write(records, rel),
                Some(From::Tree { rel: from, dir: true }) if is_dir => self.moved_within(records, &from, rel),
                Some(From::Tree { rel: from, dir: false }) if !is_dir => {
                    self.forget(records, &from);
                    self.adopt(records, rel);
                }
                Some(From::Ignored) => {
                    if is_dir {
                        self.watch_tree_rel(records, rel, false);
                    } else {
                        self.adopt(records, rel);
                    }
                }
                other => {
                    match other {
                        Some(From::Tree { rel: from, dir }) => self.left_tree(records, &from, dir),
                        Some(From::Staging { identity, .. }) => self.drop_staged(identity),
                        _ => {}
                    }
                    if !is_dir && !self.quiet {
                        // The source half was in a staging directory not yet watched, but the file was seen there.
                        if let Some(facts) = self.stat_rel(rel) {
                            if self.take_staged(&facts) {
                                self.mutagen_write(records, rel);
                                return;
                            }
                        }
                    }
                    let in_sync = self.in_cycle() && !is_dir;
                    self.left(in_sync, Some(rel));
                    if is_dir {
                        self.watch_tree_rel(records, rel, true);
                    } else {
                        self.adopt(records, rel);
                    }
                }
            }
        }

        /// What is at `rel` now, a link not followed.
        fn stat_rel(&self, rel: &str) -> Option<Facts> {
            match listing::reach(&self.root, rel) {
                Ok(Reached::Parent(parent, leaf)) => facts_at(parent.as_raw_fd(), &leaf),
                _ => None,
            }
        }

        /// The record follows what is at `rel` now.
        fn adopt(&mut self, records: &mut BTreeMap<String, u32>, rel: &str) {
            self.dirty = true;
            match self.stat_rel(rel) {
                Some(facts) if facts.regular() => {
                    records.insert(rel.to_owned(), facts.mode & 0o777);
                }
                _ => {
                    records.remove(rel);
                }
            }
        }

        fn forget(&mut self, records: &mut BTreeMap<String, u32>, rel: &str) {
            self.dirty = true;
            records.remove(rel);
            for path in inside(records, rel) {
                records.remove(&path);
            }
            for path in inside(&self.restores, rel) {
                self.restores.remove(&path);
            }
        }

        /// A tree path moved out of the tree (or out of the kept part): its records go, and so do the watches of a
        /// directory, which would otherwise keep reporting from wherever it went.
        fn left_tree(&mut self, records: &mut BTreeMap<String, u32>, rel: &str, dir: bool) {
            self.forget(records, rel);
            if dir {
                let gone: Vec<i32> = self
                    .watches
                    .iter()
                    .filter(|(_, watch)| matches!(watch, Watch::Tree { rel: path } if under(path, rel)))
                    .map(|(wd, _)| *wd)
                    .collect();
                for wd in gone {
                    self.unwatch(wd);
                    // SAFETY: removing a watch takes no pointers; one already gone is an error that changes nothing.
                    unsafe { libc::inotify_rm_watch(self.inotify.as_raw_fd(), wd) };
                }
            }
        }

        /// A directory renamed inside the tree: its watches and planned restores follow it, and its files are looked
        /// at again under the new path. An event queued for an old path (a `chmod` before the rename) found nothing
        /// there, so the record takes the modes as they are now.
        fn moved_within(&mut self, records: &mut BTreeMap<String, u32>, from: &str, to: &str) {
            self.dirty = true;
            for path in inside(&self.restores, from) {
                if let Some(restore) = self.restores.remove(&path) {
                    self.restores.insert(format!("{to}{}", &path[from.len()..]), restore);
                }
            }
            for path in inside(records, from) {
                records.remove(&path);
            }
            for watch in self.watches.values_mut() {
                if let Watch::Tree { rel } = watch {
                    if under(rel, from) {
                        *rel = format!("{to}{}", &rel[from.len()..]);
                    }
                }
            }
            self.watch_tree_rel(records, to, false);
        }

        /// A directory that appeared in the tree (or moved within it): watched, then walked, so a file put there
        /// before the watch is recorded as it is.
        fn watch_tree_rel(&mut self, records: &mut BTreeMap<String, u32>, rel: &str, arrived: bool) {
            let Ok(Reached::Parent(parent, leaf)) = listing::reach(&self.root, rel) else {
                return;
            };
            let Ok(dir) = open_dir_at(parent.as_raw_fd(), &leaf) else {
                return;
            };
            let depth = rel.split('/').count();
            let _ = self.walk_dir(records, dir, rel, depth, arrived);
        }

        /// A Mutagen write landed at `rel`: plan the restore [`wanted`] gives, only while the file is as Mutagen
        /// writes it. Otherwise somebody changed it already: the record takes it as it is, and it is counted.
        fn mutagen_write(&mut self, records: &mut BTreeMap<String, u32>, rel: &str) {
            if self.quiet {
                self.adopt(records, rel);
                return;
            }
            let Some(facts) = self.stat_rel(rel).filter(Facts::regular) else {
                self.forget(records, rel);
                return;
            };
            if !facts.as_mutagen_writes() {
                self.left(true, Some(rel));
                self.adopt(records, rel);
                return;
            }
            let current = facts.mode & 0o7777;
            let want = wanted(records.get(rel).copied(), current, self.options.umask);
            if want == current {
                self.adopt(records, rel);
                return;
            }
            self.restores.insert(rel.to_owned(), Restore { facts, want });
        }

        /// Source halves that never met their destination: a tree path moved out of the tree; a staged file that left
        /// for a directory not watched (a Mutagen write left as it was, counted when the registration names this
        /// root's staging directory, since otherwise it may have been another root's).
        fn settle_unpaired(&mut self, records: &mut BTreeMap<String, u32>) {
            for (_, from) in std::mem::take(&mut self.from) {
                match from {
                    From::Tree { rel, dir } => self.left_tree(records, &rel, dir),
                    From::Staging { landing, identity } => {
                        self.drop_staged(identity);
                        if landing && self.staging.is_some() {
                            self.left(true, None);
                        }
                    }
                    _ => {}
                }
            }
        }

        /// Puts the planned modes back, each only on the very file that was judged: same device, inode, mode and
        /// change time, one link. Linux has no compare-and-chmod, so a change between the last look and the chmod is
        /// still possible; the look after the chmod reports one it can see.
        fn apply_restores(&mut self, records: &mut BTreeMap<String, u32>) {
            for (rel, restore) in std::mem::take(&mut self.restores) {
                self.dirty = true;
                let Ok(Reached::Parent(parent, leaf)) = listing::reach(&self.root, &rel) else {
                    records.remove(&rel);
                    continue;
                };
                // SAFETY: `leaf` is NUL-terminated and outlives the call; the parent descriptor is open.
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        leaf.as_ptr(),
                        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    records.remove(&rel);
                    continue;
                }
                // SAFETY: `fd` is a fresh descriptor owned only here.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                let Some(now) = facts_fd(fd.as_raw_fd()).filter(Facts::regular) else {
                    records.remove(&rel);
                    continue;
                };
                let judged = restore.facts;
                let same = now.dev == judged.dev
                    && now.ino == judged.ino
                    && now.mode == judged.mode
                    && now.ctime == judged.ctime
                    && now.nlink == 1;
                if !same {
                    self.left(true, Some(&rel));
                    records.insert(rel, now.mode & 0o777);
                    continue;
                }
                // An O_PATH descriptor cannot be fchmod'ed; its /proc entry names exactly the inode it holds, so the
                // change cannot reach anything a link points at.
                let Ok(path) = c_name(format!("/proc/self/fd/{}", fd.as_raw_fd()).as_bytes()) else {
                    continue;
                };
                // SAFETY: `path` is NUL-terminated and outlives the call.
                if unsafe { libc::chmod(path.as_ptr(), restore.want as libc::mode_t) } != 0 {
                    records.insert(rel, now.mode & 0o777);
                    continue;
                }
                let after = facts_fd(fd.as_raw_fd());
                match after {
                    Some(after) if after.mode & 0o7777 == restore.want && after.nlink == 1 => {
                        records.insert(rel, restore.want);
                        self.shared.status().restored += 1;
                    }
                    _ => {
                        self.audit(&format!(
                            "rel={} changed around the restore",
                            super::super::audit_word(&rel)
                        ));
                        self.left(true, Some(&rel));
                        match after {
                            Some(after) => records.insert(rel, after.mode & 0o777),
                            None => records.remove(&rel),
                        };
                    }
                }
            }
        }

        fn audit(&self, what: &str) {
            let Some(state) = self.modes_dir.as_deref().and_then(Path::parent).and_then(Path::parent) else {
                return;
            };
            let _ = crate::audit::record(
                state,
                "modes-keeper",
                &format!("root={} {what}", super::super::audit_word(&self.root_text)),
            );
        }

        /// Rewrites the record file in one rename: `<rel>\0<octal mode>\n` per file, sorted by path.
        fn write_out(&mut self, records: &BTreeMap<String, u32>) {
            self.dirty = false;
            self.written = Instant::now();
            let Some(dir) = &self.modes_dir else { return };
            if !self.shared.registered(dir) {
                return;
            }
            let mut bytes = Vec::with_capacity(records.len() * 32);
            for (rel, mode) in records {
                bytes.extend_from_slice(rel.as_bytes());
                bytes.push(0);
                bytes.extend_from_slice(format!("{mode:o}\n").as_bytes());
            }
            if let Ok(dir) = agent::private_fs::open_dir(dir) {
                let _ = agent::private_fs::publish_private_file(&dir, std::ffi::OsStr::new("modes"), &bytes);
            }
        }
    }
}

/// A root's registration as `<state>/sync/<16 hex>/watch` keeps it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Registration {
    pub root: String,
    pub patterns: Vec<String>,
    /// The tree session's staging directory name in the root's parent.
    #[serde(default)]
    pub staging_name: Option<String>,
    /// The run key of the daemon that registered it: that daemon keeps the root, and any other lets it go.
    #[serde(default)]
    pub run_key: String,
}

impl Registration {
    fn same_keeping(&self, other: &Registration) -> bool {
        self.patterns == other.patterns && self.staging_name == other.staging_name
    }
}

enum Slot {
    /// Kept here. The keeper is declared before the lock, so it has stopped before the lock is let go.
    Kept {
        keeper: Keeper,
        registration: Registration,
        lock: agent::private_fs::PrivateLock,
    },
    /// Another daemon keeps the root (it holds the lock, or registered the root and runs); `registrant_gone` when the
    /// registering daemon is not running (another non-registrant holds the lock).
    Elsewhere { registrant_gone: bool },
    /// Registered, but no keeper can run for it; the line says why.
    NotKept { warning: ModeWarning },
}

/// Every root registered for mode keeping, as one daemon sees them. A registration is a file under the shared state
/// directory, so every daemon of the account (any installation) reads it at start and at each tick; the daemon that
/// registered the root keeps it, and the keeper lock makes sure only one does.
pub struct Keepers {
    state: PathBuf,
    options: Options,
    run_key: String,
    /// Held across reading or writing a registration and acting on it, so a registration and a tick never cross.
    registry: Mutex<()>,
    slots: Mutex<BTreeMap<String, Slot>>,
}

impl Keepers {
    pub fn new(state: &Path, options: Options, run_key: &str) -> Keepers {
        Keepers {
            state: state.to_path_buf(),
            options,
            run_key: run_key.to_owned(),
            registry: Mutex::new(()),
            slots: Mutex::new(BTreeMap::new()),
        }
    }

    /// The server's name, as the lines use it.
    pub fn host(&self) -> &str {
        &self.options.host
    }

    fn slots(&self) -> MutexGuard<'_, BTreeMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn registry(&self) -> MutexGuard<'_, ()> {
        self.registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn warning(&self, root: &str, text: String) -> ModeWarning {
        ModeWarning {
            host: self.options.host.clone(),
            root: root.to_owned(),
            text,
            paths: Vec::new(),
            sequence: 0,
            epoch: 0,
        }
    }

    /// Registers (`on`) or unregisters a root. A registration is kept on disk and in effect until it is turned off;
    /// the refusal is the line a person reads when the root cannot be kept.
    pub fn set(&self, root: &str, patterns: Vec<String>, staging_name: &str, on: bool) -> Result<(), String> {
        if patterns.len() > MAX_PATTERNS || patterns.iter().any(|pattern| pattern.len() > MAX_PATTERN_BYTES) {
            return Err(format!(
                "at most {MAX_PATTERNS} ignore patterns of at most {MAX_PATTERN_BYTES} bytes each"
            ));
        }
        let staging_name = match staging_name {
            "" => None,
            name => {
                check_staging_name(name)?;
                Some(name.to_owned())
            }
        };
        let _registry = self.registry();
        if !on {
            let canonical = match super::root(root) {
                Ok(held) => held.canonical,
                Err(_) if root.starts_with('/') && root.len() <= 4096 => root.to_owned(),
                Err(refusal) => return Err(refusal),
            };
            let removed = self.slots().remove(&canonical);
            // The keeper stops (writing nothing more) before the files go, and the lock is let go after both.
            let lock = match removed {
                Some(Slot::Kept { keeper, lock, .. }) => {
                    keeper.discard();
                    drop(keeper);
                    Some(lock)
                }
                _ => None,
            };
            let dir = sync_dir(&self.state, &canonical);
            for name in ["watch", "modes", "status"] {
                match std::fs::remove_file(dir.join(name)) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.to_string()),
                    _ => {}
                }
            }
            drop(lock);
            let _ = crate::audit::record(
                &self.state,
                "modes-keeper",
                &format!("root={} off", super::audit_word(&canonical)),
            );
            return Ok(());
        }
        let canonical = super::root(root)?.canonical;
        let registration = Registration {
            root: canonical.clone(),
            patterns,
            staging_name,
            run_key: self.run_key.clone(),
        };
        let bytes = serde_json::to_vec(&registration).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_REGISTRATION_BYTES {
            return Err(format!("the registration exceeds {MAX_REGISTRATION_BYTES} bytes"));
        }
        let dir = sync_dir(&self.state, &canonical);
        agent::private_fs::create_private_dir_all(&dir, &self.state).map_err(|error| error.to_string())?;
        let opened = agent::private_fs::open_dir(&dir).map_err(|error| error.to_string())?;
        agent::private_fs::publish_private_file(&opened, std::ffi::OsStr::new("watch"), &bytes)
            .map_err(|error| error.to_string())?;
        self.apply(&registration);
        match self.slots().get(&canonical) {
            Some(Slot::NotKept { warning }) => Err(warning.text.clone()),
            _ => Ok(()),
        }
    }

    /// Brings this daemon in line with the registrations on disk: keeps the roots it registered (or that name no
    /// daemon), lets go of roots another daemon registered, retries one that could not run, and stops one turned off
    /// elsewhere.
    pub fn resume(&self) {
        let _registry = self.registry();
        let registered = self.registrations();
        let gone: Vec<Slot> = {
            let mut slots = self.slots();
            let roots: Vec<String> = slots
                .keys()
                .filter(|root| !registered.iter().any(|reg| &reg.root == *root))
                .cloned()
                .collect();
            roots.iter().filter_map(|root| slots.remove(root)).collect()
        };
        // Turned off elsewhere: the keepers stop without writing under the removed registration again.
        for slot in &gone {
            if let Slot::Kept { keeper, .. } = slot {
                keeper.discard();
            }
        }
        drop(gone);
        for registration in &registered {
            self.apply(registration);
        }
    }

    fn registrations(&self) -> Vec<Registration> {
        let Ok(entries) = std::fs::read_dir(self.state.join("sync")) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path().join("watch");
            let Ok(text) = agent::private_fs::read_private_nofollow_capped(&path, MAX_REGISTRATION_BYTES as u64) else {
                continue;
            };
            let Ok(registration) = serde_json::from_str::<Registration>(&text) else {
                continue;
            };
            if entry.file_name().to_str() == Some(root_key(&registration.root).as_str()) {
                found.push(registration);
            }
        }
        found
    }

    fn apply(&self, registration: &Registration) {
        let root = &registration.root;
        let mut slots = self.slots();
        let previous = slots.remove(root);
        let registrant_gone = !registration.run_key.is_empty()
            && registration.run_key != self.run_key
            && !self.daemon_runs(&registration.run_key);
        // The registering daemon keeps the root; while it is not running, any daemon that takes the lock does.
        let mine = registration.run_key.is_empty() || registration.run_key == self.run_key || registrant_gone;
        if !mine {
            if let Some(Slot::Kept { keeper, lock, .. }) = previous {
                drop(keeper);
                drop(lock);
                let _ = crate::audit::record(
                    &self.state,
                    "modes-keeper",
                    &format!(
                        "root={} released to={}",
                        super::audit_word(root),
                        super::audit_word(&registration.run_key)
                    ),
                );
            }
            slots.insert(root.clone(), Slot::Elsewhere { registrant_gone });
            return;
        }
        let (lock, was) = match previous {
            Some(Slot::Kept {
                keeper,
                registration: kept,
                lock,
            }) => {
                if keeper.running() && kept.same_keeping(registration) {
                    slots.insert(
                        root.clone(),
                        Slot::Kept {
                            keeper,
                            registration: registration.clone(),
                            lock,
                        },
                    );
                    return;
                }
                drop(keeper);
                (lock, None)
            }
            other => {
                let was = match other {
                    Some(Slot::NotKept { warning }) => Some(warning),
                    _ => None,
                };
                match self.take_lock(root) {
                    Ok(Some(lock)) => (lock, was),
                    Ok(None) => {
                        slots.insert(root.clone(), Slot::Elsewhere { registrant_gone });
                        return;
                    }
                    Err(reason) => {
                        let warning = self.warning(root, not_kept(&self.options.host, root, &reason));
                        self.audit_not_kept(root, &reason, was.as_ref(), &warning);
                        slots.insert(root.clone(), Slot::NotKept { warning });
                        return;
                    }
                }
            }
        };
        let dir = sync_dir(&self.state, root);
        let ignored = ignore_for(&registration.patterns);
        match Keeper::start(
            root,
            registration.staging_name.clone(),
            ignored,
            self.options.clone(),
            Some(dir),
        ) {
            Ok(keeper) => {
                let _ = crate::audit::record(
                    &self.state,
                    "modes-keeper",
                    &format!("root={} kept", super::audit_word(root)),
                );
                slots.insert(
                    root.clone(),
                    Slot::Kept {
                        keeper,
                        registration: registration.clone(),
                        lock,
                    },
                );
            }
            Err(reason) => {
                let warning = self.warning(root, not_kept(&self.options.host, root, &reason));
                self.audit_not_kept(root, &reason, was.as_ref(), &warning);
                slots.insert(root.clone(), Slot::NotKept { warning });
            }
        }
    }

    /// Whether the daemon with run key `run_key` runs: its run directory's `info` names a live process. (The `info`
    /// goes when a daemon exits; a crashed one leaves a dead pid.) The lifetime lock is not probed: a probe holding it
    /// for an instant could make that daemon's own start fail. A key that cannot name a run directory, or an `info`
    /// that cannot be read for another reason than its absence, counts as running, and the root is left to it.
    fn daemon_runs(&self, run_key: &str) -> bool {
        if run_key.is_empty() || run_key.contains('/') || run_key == "." || run_key == ".." {
            return true;
        }
        let path = self.state.join("run").join(run_key).join("info");
        match agent::private_fs::read_private_nofollow_capped(&path, 16 * 1024) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(_) => true,
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|info| info.get("pid")?.as_u64())
            {
                Some(pid) => u32::try_from(pid).map_or(true, crate::run_dir::pid_alive),
                None => true,
            },
        }
    }

    /// Whether some daemon holds `root`'s keeper lock now. A shared look, given back at once.
    fn lock_held(&self, root: &str) -> bool {
        let path = sync_dir(&self.state, root).join("keeper.lock");
        match agent::private_fs::PrivateLock::open_existing(&path) {
            Ok(lock) => !matches!(lock.try_lock_shared(), Ok(true)),
            Err(_) => false,
        }
    }

    fn take_lock(&self, root: &str) -> Result<Option<agent::private_fs::PrivateLock>, String> {
        let dir = sync_dir(&self.state, root);
        agent::private_fs::create_private_dir_all(&dir, &self.state).map_err(|error| error.to_string())?;
        let lock = agent::private_fs::PrivateLock::open(&dir.join("keeper.lock")).map_err(|error| error.to_string())?;
        match lock.try_lock() {
            Ok(true) => Ok(Some(lock)),
            Ok(false) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn audit_not_kept(&self, root: &str, reason: &str, was: Option<&ModeWarning>, now: &ModeWarning) {
        if was != Some(now) {
            let _ = crate::audit::record(
                &self.state,
                "modes-keeper",
                &format!(
                    "root={} not-kept {}",
                    super::audit_word(root),
                    super::audit_word(reason)
                ),
            );
        }
    }

    /// Whether a keeper of this daemon runs: the daemon then stays past its idle exit.
    pub fn holds(&self) -> bool {
        self.slots()
            .values()
            .any(|slot| matches!(slot, Slot::Kept { keeper, .. } if keeper.running()))
    }

    /// Whether this daemon keeps `root` now.
    #[allow(dead_code)] // for tests
    pub fn keeps(&self, root: &str) -> bool {
        matches!(self.slots().get(root), Some(Slot::Kept { keeper, .. }) if keeper.running())
    }

    /// Runs `look` on this daemon's keeper of `root`, if it has one.
    #[allow(dead_code)] // for tests
    pub fn with_keeper<T>(&self, root: &str, look: impl FnOnce(&Keeper) -> T) -> Option<T> {
        match self.slots().get(root) {
            Some(Slot::Kept { keeper, .. }) => Some(look(keeper)),
            _ => None,
        }
    }

    /// The lines Hello carries: roots that cannot be kept, what a running keeper could not do (its counted writes
    /// reported once), and the lines of roots another daemon keeps, from their status files.
    pub fn hello_warnings(&self) -> Vec<ModeWarning> {
        let mut lines = Vec::new();
        let mut relay = Vec::new();
        for (root, slot) in self.slots().iter() {
            match slot {
                Slot::NotKept { warning } => lines.push(warning.clone()),
                Slot::Kept { keeper, .. } => lines.extend(keeper.warnings()),
                Slot::Elsewhere { registrant_gone } => relay.push((root.clone(), *registrant_gone)),
            }
        }
        for (root, registrant_gone) in relay {
            let path = sync_dir(&self.state, &root).join("status");
            let file = agent::private_fs::read_private_nofollow_capped(&path, 1024 * 1024)
                .ok()
                .and_then(|text| serde_json::from_str::<StatusFile>(&text).ok());
            match file {
                // Passed on as the keeping daemon wrote it: the laptop tells new counted lines by their sequence.
                Some(file) => {
                    lines.extend(file.warnings);
                    lines.extend(file.counted);
                }
                // Unless another daemon holds the root's keeper lock (its keeper may still be walking).
                None if registrant_gone && !self.lock_held(&root) => lines.push(self.warning(
                    &root,
                    not_kept(
                        &self.options.host,
                        &root,
                        "the daemon that registered it is not running",
                    ),
                )),
                None => {}
            }
        }
        lines
    }

    /// Stops every keeper and lets their locks go; the registrations stay for the next daemon.
    pub fn stop_all(&self) {
        let stopped = std::mem::take(&mut *self.slots());
        drop(stopped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(mode: u32, nlink: u32, btime: Option<(i64, u32)>) -> Facts {
        Facts {
            dev: 1,
            ino: 2,
            mode: libc::S_IFREG | mode,
            nlink,
            ctime: (5, 6),
            btime,
        }
    }

    #[test]
    fn an_identity_matches_only_with_the_same_birth_time_and_mutagens_mode() {
        let staged: HashMap<Identity, u32> = [((1, 2, (10, 7)), 1)].into_iter().collect();
        assert_eq!(
            staged_match(&staged, &facts(0o600, 1, Some((10, 7)))),
            Some((1, 2, (10, 7)))
        );
        // The inode used again by another file: another birth time.
        assert_eq!(staged_match(&staged, &facts(0o600, 1, Some((10, 8)))), None);
        assert_eq!(staged_match(&staged, &facts(0o600, 1, Some((11, 7)))), None);
        // No birth time from the file system: no identity at all.
        assert_eq!(staged_match(&staged, &facts(0o600, 1, None)), None);
        // Changed since Mutagen wrote it.
        assert_eq!(staged_match(&staged, &facts(0o640, 1, Some((10, 7)))), None);
        assert_eq!(staged_match(&staged, &facts(0o600, 2, Some((10, 7)))), None);
    }
}
