//! The shadow repository: Eitri's own git directory per project, holding a snapshot of the work
//! tree at the start and the end of every agent turn.
//!
//! It lives under `<state home>/eitri/review/<project key>/`, never in the project, and it never
//! reads or writes the project's own `.git` beyond two read-only questions (where its common
//! directory is, and what its `info/exclude` says). Its layout:
//!
//! - `git/` -- a bare git directory created with an empty template (so it has no hooks), its
//!   files 0600 and directories 0700. Its config turns line-ending conversion, fsmonitor and
//!   automatic gc off, and its `info/attributes` turns off every attribute that could make a blob
//!   differ from the bytes on disk; that file outranks every `.gitattributes` in the work tree.
//! - `index-<session>` -- one index per provider session, so two sessions snapshotting at once
//!   never contend for one index lock. Object writes are content-addressed and safe to share.
//! - `index-<session>.guard` -- an `flock` held exclusively by whichever snapshot of that session
//!   is running (the same session can be open in two windows). Holding it is what makes an
//!   `index-<session>.lock` or a lock on one of the session's refs provably stale: only a git run
//!   that was killed before it could remove its lock leaves one there, and nothing else will.
//! - `lock` -- an `flock` shared by everything that reads or writes objects and refs, and taken
//!   exclusively only to delete refs and objects, so a collection never removes an object a
//!   concurrent snapshot (in this window or another one on the same project) has written but not
//!   yet referenced. The slow part of a collection, repacking, deletes nothing and runs under the
//!   lock shared, beside snapshots ([`Shadow::trim`]).
//! - `collect.lock` -- an `flock` held exclusively by the one collection of the project that is
//!   running; another window's waits for it.
//!
//! Each snapshot is a root commit under `refs/eitri/<session>/<turn>/<base|end|warm>`, its
//! message the [`SnapshotLabel`] as `key: value` lines, then when it finished and the files it left
//! out for their size. A sibling `<kind>-ignored` ref retains its ignored-path blob for deletion
//! checks. What a turn's review must say about its precision is a commit of the empty
//! tree under `refs/eitri/<session>/<turn>/marks` ([`TurnMarks`]). There is no other state: the refs are the
//! index of what exists, and deleting a ref really frees its snapshot, since no parent chain keeps
//! it alive.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use super::git::{self, GitError};

/// What the shadow's `info/attributes` says: no text conversion, no filter, no `$Id$` expansion
/// and no encoding conversion, for every path.
pub const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding\n";

/// How long a blob read, a lock wait outside a snapshot, or opening the store may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Larger ignored-path lists leave deletion unavailable, without losing the snapshot's view.
const MAX_IGNORED_BYTES: usize = 4 * 1024 * 1024;

/// Ignored paths retained with a snapshot, including collapsed excluded directories.
pub(super) struct RecordedIgnored(Vec<u8>);

impl RecordedIgnored {
    /// ASCII folding can only add refusals on filesystems that fold path names.
    pub(super) fn contains(&self, path: &Path) -> bool {
        let path = path.as_os_str().as_bytes();
        nul_separated(&self.0).any(|entry| {
            entry.eq_ignore_ascii_case(path)
                || (entry.ends_with(b"/")
                    && path
                        .get(..entry.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(entry)))
        })
    }

    /// Ask the volume about aliases too: its case and normalization rules may exceed ASCII folding.
    pub(super) fn contains_with_stat(
        &self,
        path: &Path,
        mut stat: impl FnMut(&Path) -> std::io::Result<Option<(u64, u64)>>,
    ) -> std::io::Result<bool> {
        if self.contains(path) {
            return Ok(true);
        }
        for entry in nul_separated(&self.0) {
            let directory = entry.ends_with(b"/");
            let entry = Path::new(OsStr::from_bytes(entry.strip_suffix(b"/").unwrap_or(entry)));
            for candidate in path.ancestors().take_while(|p| !p.as_os_str().is_empty()) {
                if (candidate == path || directory) && candidate.parent() == entry.parent() {
                    if let Some(identity) = stat(candidate)? {
                        if stat(entry)? == Some(identity) {
                            return Ok(true);
                        }
                    }
                    break;
                }
            }
        }
        Ok(false)
    }

    /// A listed child proves its ancestors were directories, even without any tracked files.
    /// Positive evidence uses exact bytes: case folding must never authorize deletion.
    pub(super) fn contains_directory(&self, path: &Path) -> bool {
        let mut prefix = path.as_os_str().as_bytes().to_vec();
        prefix.push(b'/');
        nul_separated(&self.0).any(|entry| entry.len() > prefix.len() && entry.starts_with(&prefix))
    }
}

/// The limits one snapshot works within. A snapshot that cannot stay inside them says so instead
/// of presenting a partial tree as complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// A file larger than this is left out of the snapshot and listed in
    /// [`Snapshot::skipped_large`].
    pub max_file_bytes: u64,
    /// More files than this in the whole snapshot leaves the turn without a snapshot.
    pub max_files: usize,
    /// The whole snapshot, lock wait included, must finish within this.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 8 * 1024 * 1024,
            max_files: 20_000,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Which point of a turn a snapshot records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SnapshotKind {
    /// When the turn started.
    Base,
    /// When the turn ended.
    End,
    /// When the session started, so the first turn's base has an index to start from.
    Warm,
}

impl SnapshotKind {
    /// The ref's last component and the label's `kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotKind::Base => "base",
            SnapshotKind::End => "end",
            SnapshotKind::Warm => "warm",
        }
    }

    fn parse(s: &str) -> Option<SnapshotKind> {
        match s {
            "base" => Some(SnapshotKind::Base),
            "end" => Some(SnapshotKind::End),
            "warm" => Some(SnapshotKind::Warm),
            _ => None,
        }
    }
}

/// What a snapshot is of, written into its commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotLabel {
    /// The turn's number within its session (the ref's `<n>`).
    pub turn: u32,
    pub kind: SnapshotKind,
    /// The provider's turn id, when there is one. Line breaks and NULs are replaced by spaces.
    pub turn_id: String,
    pub tab: u64,
    /// Wall-clock time the snapshot was asked for, in milliseconds since the Unix epoch.
    pub time_ms: u64,
}

impl SnapshotLabel {
    /// The commit message: one `key: value` line per field.
    pub fn to_message(&self) -> String {
        let turn_id: String = self
            .turn_id
            .chars()
            .map(|c| if c == '\n' || c == '\r' || c == '\0' { ' ' } else { c })
            .collect();
        format!(
            "kind: {}\nturn: {}\nturn-id: {}\ntab: {}\ntime: {}\n",
            self.kind.as_str(),
            self.turn,
            turn_id,
            self.tab,
            self.time_ms
        )
    }

    /// Reads a message written by [`to_message`](Self::to_message) back. `None` when a field is
    /// missing or malformed.
    pub fn parse(message: &str) -> Option<SnapshotLabel> {
        let mut fields = BTreeMap::new();
        for line in message.lines() {
            if let Some((key, value)) = line.split_once(": ") {
                fields.insert(key, value);
            } else if let Some(key) = line.strip_suffix(':') {
                fields.insert(key, "");
            }
        }
        Some(SnapshotLabel {
            kind: SnapshotKind::parse(fields.get("kind")?)?,
            turn: fields.get("turn")?.parse().ok()?,
            turn_id: fields.get("turn-id")?.to_string(),
            tab: fields.get("tab")?.parse().ok()?,
            time_ms: fields.get("time")?.parse().ok()?,
        })
    }
}

/// What a snapshot's message says after its label: when it finished, and every file it left out for
/// its size. A later run rebuilds a turn from its refs alone, and without these it would date a
/// late baseline wrongly and show a file left out of one snapshot as added or deleted.
fn snapshot_message(label: &SnapshotLabel, taken_ms: u64, skipped_large: &[(PathBuf, Option<FilePrint>)]) -> String {
    let mut message = label.to_message();
    message.push_str(&format!("taken: {taken_ms}\n"));
    for (path, print) in skipped_large {
        // Hex, because a file name may hold a line break or bytes that are not UTF-8.
        let name = hex_encode(path.as_os_str().as_bytes());
        message.push_str("skipped-large: ");
        message.push_str(&name);
        message.push('\n');
        // A line of its own, so a reader from before it existed still finds every `skipped-large`
        // line it knows and ignores this one.
        if let Some(print) = print {
            message.push_str(&format!("skipped-print: {name} {}\n", print.to_field()));
        }
    }
    message
}

/// The `taken` line of a snapshot's message, when it has one.
fn parse_taken(message: &str) -> Option<u64> {
    message
        .lines()
        .find_map(|line| line.strip_prefix("taken: "))
        .and_then(|value| value.parse().ok())
}

/// The `skipped-large` lines of a snapshot's message. A line that does not decode is dropped.
fn parse_skipped_large(message: &str) -> Vec<PathBuf> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix("skipped-large: "))
        .filter_map(hex_decode)
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes)))
        .collect()
}

/// The files a snapshot's message says it left out for their size, each with the print it was
/// taken with. A message from before prints were kept has none, so each file reads as one whose
/// change cannot be ruled out; a `skipped-print` line that does not decode is dropped the same way.
fn parse_left_out(message: &str) -> LeftOut {
    let mut prints: BTreeMap<PathBuf, FilePrint> = BTreeMap::new();
    for line in message.lines() {
        let Some(rest) = line.strip_prefix("skipped-print: ") else {
            continue;
        };
        let Some((name, fields)) = rest.split_once(' ') else {
            continue;
        };
        if let (Some(name), Some(print)) = (hex_decode(name), FilePrint::from_field(fields)) {
            prints.insert(PathBuf::from(OsString::from_vec(name)), print);
        }
    }
    parse_skipped_large(message)
        .into_iter()
        .map(|path| {
            let print = prints.get(&path).copied();
            (path, print)
        })
        .collect()
}

/// What can be compared about a file without reading its bytes: its size, when its content and its
/// inode last changed, and which inode it is. A snapshot keeps one for each file it leaves out for
/// its size, so a later review can tell a file that was left out of both of a turn's snapshots and
/// did not change from one that did. A write changes the modification time, and nothing a program
/// can do without privileges sets the change time back; a replacement by rename changes the inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePrint {
    pub len: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub ino: u64,
}

impl FilePrint {
    pub fn of(meta: &std::fs::Metadata) -> FilePrint {
        FilePrint {
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
        }
    }

    /// The print of `relative` under `work_tree` as it is now; `None` for a path that is not there
    /// or not a regular file. Never follows a final symlink.
    pub fn of_path(work_tree: &Path, relative: &Path) -> Option<FilePrint> {
        let meta = work_tree.join(relative).symlink_metadata().ok()?;
        meta.is_file().then(|| FilePrint::of(&meta))
    }

    fn to_field(self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.len, self.mtime.0, self.mtime.1, self.ctime.0, self.ctime.1, self.ino
        )
    }

    fn from_field(text: &str) -> Option<FilePrint> {
        let mut parts = text.split(' ');
        let print = FilePrint {
            len: parts.next()?.parse().ok()?,
            mtime: (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?),
            ctime: (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?),
            ino: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(print)
    }
}

/// The files one snapshot left out for their size, with the print each was taken with, `None` where
/// the snapshot's record has none (one written before prints were kept).
pub type LeftOut = BTreeMap<PathBuf, Option<FilePrint>>;

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || text.is_empty() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What a turn's review must say about its own precision, kept beside its snapshots so a later run
/// says it too. These are learnt while the turn runs, and some only after its end snapshot was
/// asked for (the next turn starting on top of it, another tab's turn starting), so they are a ref
/// of their own, `refs/eitri/<session>/<turn>/marks`, rewritten whole whenever one is learnt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnMarks {
    /// A tool call ran before the base snapshot was done.
    pub late: bool,
    /// The next turn's tools started before the end snapshot was done.
    pub overlapped_next: bool,
    /// Another tab's turn on the same project ran at the same time.
    pub overlapped_tab: bool,
    /// The agent still had a command running when the turn ended: what it wrote afterwards is in
    /// no snapshot of this turn.
    pub left_running: bool,
}

impl TurnMarks {
    pub fn any(&self) -> bool {
        self.late || self.overlapped_next || self.overlapped_tab || self.left_running
    }

    fn to_message(self, turn: u32, turn_id: &str) -> String {
        let turn_id: String = turn_id
            .chars()
            .map(|c| if c == '\n' || c == '\r' || c == '\0' { ' ' } else { c })
            .collect();
        format!(
            "kind: marks\nturn: {turn}\nturn-id: {turn_id}\nlate: {}\noverlapped-next: {}\noverlapped-tab: {}\nleft-running: {}\n",
            self.late, self.overlapped_next, self.overlapped_tab, self.left_running
        )
    }

    /// Reads a message written by `to_message` back; `None` for any other message. A mark a message
    /// does not name (one written before that mark existed) reads as not set.
    fn parse(message: &str) -> Option<TurnMarks> {
        let mut fields = BTreeMap::new();
        for line in message.lines() {
            if let Some((key, value)) = line.split_once(": ") {
                fields.insert(key, value);
            }
        }
        if fields.get("kind") != Some(&"marks") {
            return None;
        }
        let flag = |key: &str| fields.get(key) == Some(&"true");
        Some(TurnMarks {
            late: flag("late"),
            overlapped_next: flag("overlapped-next"),
            overlapped_tab: flag("overlapped-tab"),
            left_running: flag("left-running"),
        })
    }
}

/// One snapshot that was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The root commit holding it.
    pub commit: String,
    /// Its tree.
    pub tree: String,
    /// `refs/eitri/<session>/<turn>/<kind>`.
    pub reference: String,
    /// Files over [`Limits::max_file_bytes`], relative to the work tree: left out of the tree.
    pub skipped_large: Vec<PathBuf>,
    /// The same files with the print each had when the snapshot was taken.
    pub left_out: LeftOut,
}

/// What a path is in a snapshot, as [`Shadow::read_entry`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    Blob,
    /// A symbolic link; the entry's bytes are its target.
    Symlink,
    /// A nested repository's commit; the entry's bytes are the commit id, since its objects are not
    /// in the shadow.
    Gitlink,
}

/// One path of a snapshot: its kind, its permission bits and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Permission bits, ready to be given to a file: git records only 0644 and 0755 for a file; a
    /// symlink is 0777 and a gitlink 0.
    pub mode: u32,
    pub kind: EntryKind,
    pub bytes: Vec<u8>,
}

/// One snapshot ref as [`Shadow::snapshot_ref_ids`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRef {
    /// `refs/eitri/<session>/<turn>/<kind>`.
    pub name: String,
    /// The commit it points at.
    pub commit: String,
    /// Its label, when the commit message is one.
    pub label: Option<SnapshotLabel>,
    /// When the snapshot finished, when its message says.
    pub taken_ms: Option<u64>,
    /// The files the snapshot left out for their size, as its message lists them.
    pub skipped_large: Vec<PathBuf>,
    /// The same files with their prints, where the message has them.
    pub left_out: LeftOut,
    /// For a `marks` ref: what it says about its turn.
    pub marks: Option<TurnMarks>,
}

impl SnapshotRef {
    /// `(session, turn, kind)` from the ref's name, when it has that shape.
    pub fn parts(&self) -> Option<(&str, u32, SnapshotKind)> {
        let (session, turn, kind) = self.name_parts()?;
        Some((session, turn, SnapshotKind::parse(kind)?))
    }

    /// `(session, turn)` of any ref under a turn, its `marks` included.
    pub fn session_turn(&self) -> Option<(&str, u32)> {
        let (session, turn, kind) = self.name_parts()?;
        (SnapshotKind::parse(kind).is_some() || kind == MARKS).then_some((session, turn))
    }

    fn name_parts(&self) -> Option<(&str, u32, &str)> {
        let mut parts = self.name.strip_prefix("refs/eitri/")?.split('/');
        let (session, turn, kind) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Some((session, turn.parse().ok()?, kind))
    }
}

/// The last component of a turn's marks ref.
const MARKS: &str = "marks";

/// What [`Shadow::snapshot`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Taken(Snapshot),
    /// No snapshot exists for this label; the reason is for the user.
    Unavailable(String),
}

/// Why a shadow operation failed.
#[derive(Debug)]
pub enum ShadowError {
    Io(std::io::Error),
    Git(GitError),
    /// The operation's time ran out before it finished.
    TimedOut,
    /// More files in the whole snapshot than [`Limits::max_files`].
    TooManyFiles {
        count: usize,
        max: usize,
    },
    /// A session id that cannot be a file name and a ref component.
    InvalidSession(String),
    /// A commit id that is not hex, or does not name a commit in the shadow.
    NoSuchCommit(String),
    /// A path that is absolute or empty where a work-tree-relative one is needed.
    InvalidPath(PathBuf),
    /// A revert stamp, a journal id or a blob id that cannot be what it is meant to be.
    InvalidName(String),
}

impl std::fmt::Display for ShadowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShadowError::Io(e) => write!(f, "the review store could not be used: {e}"),
            ShadowError::Git(e) => write!(f, "{e}"),
            ShadowError::TimedOut => write!(f, "the snapshot took too long"),
            ShadowError::TooManyFiles { count, max } => {
                write!(f, "too many files to snapshot ({count}, the limit is {max})")
            }
            ShadowError::InvalidSession(s) => write!(f, "session id {s:?} cannot name a snapshot"),
            ShadowError::NoSuchCommit(c) => write!(f, "no snapshot {c:?}"),
            ShadowError::InvalidPath(p) => write!(f, "{} is not a path inside the project", p.display()),
            ShadowError::InvalidName(s) => write!(f, "{s:?} cannot name a stored revert"),
        }
    }
}

impl std::error::Error for ShadowError {}

impl From<std::io::Error> for ShadowError {
    fn from(e: std::io::Error) -> Self {
        ShadowError::Io(e)
    }
}

impl From<GitError> for ShadowError {
    fn from(e: GitError) -> Self {
        match e {
            GitError::TimedOut => ShadowError::TimedOut,
            other => ShadowError::Git(other),
        }
    }
}

/// `<state home>/eitri/review/<project key>` for the canonical `project_root`: the same 16-hex key
/// `layout/` and the other per-project state use. `None` when neither variable gives a state
/// directory.
pub fn review_dir_for(state_home: Option<&OsStr>, home: Option<&OsStr>, project_root: &Path) -> Option<PathBuf> {
    let base = crate::state_paths::state_subdir(state_home, home, "review")?;
    Some(base.join(crate::state_paths::project_key(project_root)))
}

/// A held `flock` on the store's `lock` file, released on drop.
#[derive(Debug)]
pub struct ShadowLock {
    file: File,
}

impl ShadowLock {
    /// Whether `path` still names the file this lock is held on, as a regular file and not through
    /// a symlink. A file another process unlinked or replaced after it was opened but before it was
    /// locked is still locked, and nobody looking at the path would ever meet that lock.
    pub(crate) fn names(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let (Ok(held), Ok(named)) = (self.file.metadata(), std::fs::symlink_metadata(path)) else {
            return false;
        };
        named.is_file() && held.dev() == named.dev() && held.ino() == named.ino()
    }
}

/// One project's shadow repository. Cheap to clone; every method takes the lock it needs.
#[derive(Debug, Clone)]
pub struct Shadow {
    review_dir: PathBuf,
    git_dir: PathBuf,
    work_tree: PathBuf,
    excludes_file: Option<PathBuf>,
    user_excludes: bool,
}

/// The most of a project's `info/exclude` a snapshot copies; a larger one fails the snapshot.
const MAX_PROJECT_EXCLUDE_BYTES: u64 = 1 << 20;

/// Reads the project's `info/exclude`, which the project (and so the agent) controls: only a regular
/// file, opened without blocking and read within [`MAX_PROJECT_EXCLUDE_BYTES`]. A FIFO put in its place
/// would hold the snapshot (and the store's lock with it) for good, and a link to `/dev/zero` would
/// read until memory ran out; either fails the snapshot instead.
fn read_project_exclude(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROJECT_EXCLUDE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROJECT_EXCLUDE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is larger than {MAX_PROJECT_EXCLUDE_BYTES} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

impl Shadow {
    /// Opens (creating on first use) the store in `review_dir` for the project at `work_tree`,
    /// honouring the user's global excludes file.
    pub fn open(review_dir: &Path, work_tree: &Path) -> Result<Shadow, ShadowError> {
        let mut shadow = Self::open_with_excludes(review_dir, work_tree, git::user_excludes_file())?;
        shadow.user_excludes = true;
        Ok(shadow)
    }

    /// [`open`](Self::open) with the global excludes file given rather than read from the user's
    /// config: what tests use, so the result does not depend on the machine running them.
    pub fn open_with_excludes(
        review_dir: &Path,
        work_tree: &Path,
        excludes_file: Option<PathBuf>,
    ) -> Result<Shadow, ShadowError> {
        if !review_dir.is_absolute() || !work_tree.is_absolute() {
            return Err(ShadowError::InvalidPath(if review_dir.is_absolute() {
                work_tree.to_path_buf()
            } else {
                review_dir.to_path_buf()
            }));
        }
        agent::private_fs::create_private_dir_all(review_dir, private_root(review_dir))?;
        let shadow = Shadow {
            review_dir: review_dir.to_path_buf(),
            git_dir: review_dir.join("git"),
            work_tree: work_tree.to_path_buf(),
            excludes_file,
            user_excludes: false,
        };
        let attributes = shadow.git_dir.join("info/attributes");
        if std::fs::read(&attributes).ok().as_deref() != Some(ATTRIBUTES.as_bytes()) {
            let _lock = shadow.lock_exclusive()?;
            shadow.initialise()?;
        }
        Ok(shadow)
    }

    /// Creates the git directory and its settings. Idempotent; `info/attributes` is written last,
    /// so its presence means everything before it is done. The caller holds the lock exclusively,
    /// so a lock file left by an earlier setup that was killed (a `config.lock`) is cleared first.
    fn initialise(&self) -> Result<(), ShadowError> {
        let deadline = Instant::now() + READ_TIMEOUT;
        self.clear_stale_locks()?;
        self.run_ok("init", git::init_bare(&self.git_dir), None, deadline)?;
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.fsmonitor", "false"),
            ("core.logAllRefUpdates", "false"),
            ("gc.auto", "0"),
        ] {
            let mut cmd = self.git(None);
            cmd.args(["config", key, value]);
            self.run_ok("config", cmd, None, deadline)?;
        }
        let info = self.git_dir.join("info");
        agent::private_fs::create_private_dir_all(&info, &self.review_dir)?;
        write_atomically(&info.join("attributes"), ATTRIBUTES.as_bytes())?;
        Ok(())
    }

    /// The store's directory, `<state home>/eitri/review/<key>`.
    pub fn review_dir(&self) -> &Path {
        &self.review_dir
    }

    /// The shadow git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The project root the snapshots are of.
    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// The index file of `session`'s snapshots.
    pub fn index_path(&self, session: &str) -> Result<PathBuf, ShadowError> {
        validate_session(session)?;
        Ok(self.review_dir.join(format!("index-{session}")))
    }

    /// An isolated git command on this store ([`git::command`]), with `index` as its index file.
    pub fn git(&self, index: Option<&Path>) -> Command {
        git::command(&self.git_dir, &self.work_tree, index, self.excludes_file.as_deref())
    }

    /// Runs `cmd` with what is left until `deadline`, failing unless git succeeded.
    pub fn run_ok(
        &self,
        what: &str,
        cmd: Command,
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<Output, ShadowError> {
        let output = git::run_with_input(cmd, input, remaining(deadline)?)?;
        Ok(GitError::check(what, output)?)
    }

    /// Holds the store's lock shared until the guard drops, waiting as long as it takes.
    pub fn lock_shared(&self) -> Result<ShadowLock, ShadowError> {
        self.lock(libc::LOCK_SH, None)
    }

    /// Holds the store's lock exclusively until the guard drops, waiting as long as it takes.
    pub fn lock_exclusive(&self) -> Result<ShadowLock, ShadowError> {
        self.lock(libc::LOCK_EX, None)
    }

    /// Holds the lock shared, giving up with [`ShadowError::TimedOut`] at `deadline`.
    pub fn lock_shared_until(&self, deadline: Instant) -> Result<ShadowLock, ShadowError> {
        self.lock(libc::LOCK_SH, Some(deadline))
    }

    fn lock(&self, operation: libc::c_int, deadline: Option<Instant>) -> Result<ShadowLock, ShadowError> {
        flock_file(&self.review_dir.join("lock"), operation, deadline)
    }

    /// Holds `session`'s guard (see the module doc) exclusively, giving up at `deadline`. Taken
    /// only while the store lock is held, so a collection, which holds that lock exclusively while
    /// it removes indexes, never meets a held guard and may remove the file.
    fn lock_session_until(&self, session: &str, deadline: Instant) -> Result<ShadowLock, ShadowError> {
        validate_session(session)?;
        flock_file(
            &self.review_dir.join(format!("index-{session}.guard")),
            libc::LOCK_EX,
            Some(deadline),
        )
    }

    /// Removes every lock file a killed git run can leave in the store: top-level ones in the git
    /// directory (`config.lock`, `packed-refs.lock`), any under `refs/` and `objects/info/`, and
    /// the sessions' `index-*.lock`. git never removes such a file itself, and while it is there
    /// every later run that needs the same lock fails. Only called with the store lock held
    /// exclusively: then no git run of any window is using the store, and every lock file found
    /// is one a killed run left.
    ///
    /// In the review directory itself only the indexes' lock files go. The store lock does not
    /// keep out the project's write slot (`write.lock`, held by a revert in any window): unlinking
    /// it while held would let the next window lock a new file beside the old one, and two
    /// writes would run at once.
    fn clear_stale_locks(&self) -> std::io::Result<()> {
        remove_index_locks(&self.review_dir)?;
        remove_lock_files(&self.git_dir, false)?;
        remove_lock_files(&self.git_dir.join("refs"), true)?;
        remove_lock_files(&self.git_dir.join("objects/info"), true)?;
        Ok(())
    }

    /// Copies the project's `info/exclude` into the shadow's, since a separate git directory never
    /// reads the project's own. For a linked worktree the file is in the common git directory,
    /// which is asked of the user's own git (read-only). A project that is not the top of a git
    /// work tree has no such file to honour, and any copy left from before is removed.
    pub fn sync_excludes(&self) -> Result<(), ShadowError> {
        self.sync_excludes_until(Instant::now() + READ_TIMEOUT)
    }

    fn sync_excludes_until(&self, deadline: Instant) -> Result<(), ShadowError> {
        let target = self.git_dir.join("info/exclude");
        let source = self.project_exclude_file(deadline)?;
        let wanted = match source {
            Some(path) => match read_project_exclude(&path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            },
            None => None,
        };
        let current = std::fs::read(&target).ok();
        match wanted {
            Some(bytes) if current.as_deref() != Some(bytes.as_slice()) => write_atomically(&target, &bytes)?,
            None if current.is_some() => std::fs::remove_file(&target)?,
            _ => {}
        }
        Ok(())
    }

    /// Fingerprints the external ignore sources git reads, including absence and the global path.
    pub(super) fn exclude_digest(&self) -> Result<String, ShadowError> {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        let global = self
            .excludes_file
            .clone()
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/git/ignore")));
        for path in [Some(self.git_dir.join("info/exclude")), global] {
            if let Some(path) = path {
                let name = path.as_os_str().as_bytes();
                digest.update((name.len() as u64).to_le_bytes());
                digest.update(name);
                match read_project_exclude(&self.work_tree.join(path)) {
                    Ok(bytes) => {
                        digest.update([1]);
                        digest.update((bytes.len() as u64).to_le_bytes());
                        digest.update(bytes);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => digest.update([0]),
                    Err(e) => return Err(e.into()),
                }
            } else {
                digest.update([2]);
            }
        }
        Ok(hex_encode(&digest.finalize()))
    }

    /// The saved deletion evidence; older snapshots deliberately have none.
    pub(super) fn deletion_provenance(&self, commit: &str) -> Result<Option<(u128, String)>, ShadowError> {
        let mut cmd = self.git(None);
        cmd.args(["show", "-s", "--format=%B", commit]);
        let output = self.run_ok("show", cmd, None, Instant::now() + READ_TIMEOUT)?;
        Ok(String::from_utf8_lossy(&output.stdout).lines().find_map(|line| {
            let (time, digest) = line.strip_prefix("delete-provenance: ")?.split_once(' ')?;
            (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| Some((time.parse().ok()?, digest.to_owned())))?
        }))
    }

    /// The recorded ignored set includes rules that were themselves ignored or too large to keep.
    /// Missing evidence refuses deletion; a directory entry covers every path below it.
    pub(super) fn recorded_ignored(&self, commit: &str) -> Result<Option<RecordedIgnored>, ShadowError> {
        let mut cmd = self.git(None);
        cmd.args(["show", "-s", "--format=%B", commit]);
        let output = self.run_ok("show", cmd, None, Instant::now() + READ_TIMEOUT)?;
        let message = String::from_utf8_lossy(&output.stdout);
        let Some(blob) = message.lines().find_map(|line| line.strip_prefix("delete-ignored: ")) else {
            return Ok(None);
        };
        if !is_full_id(blob) || !matches!(self.stored_len(blob)?, Some(len) if len <= MAX_IGNORED_BYTES as u64) {
            return Ok(None);
        }
        let Some(bytes) = self.read_stored(blob)? else {
            return Ok(None);
        };
        if !bytes.is_empty() && !bytes.ends_with(b"\0") {
            return Ok(None);
        }
        Ok(Some(RecordedIgnored(bytes)))
    }

    fn list_ignored(&self, index: &Path, deadline: Instant) -> Result<BTreeSet<Vec<u8>>, ShadowError> {
        let mut cmd = self.git(Some(index));
        cmd.args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ]);
        let output = self.run_ok("ls-files", cmd, None, deadline)?;
        let mut entries: Vec<_> = nul_separated(&output.stdout).collect();
        entries.sort_unstable();
        entries.dedup();
        // Git also emits dir/ for a directory holding only ignored contents. Those contents
        // follow it in this same listing; a truly excluded directory is collapsed without them.
        // Checking a later check-ignore result instead could race a changed in-tree ignore rule.
        Ok(entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let has_children = entries.get(i + 1).is_some_and(|next| next.starts_with(entry));
                (!entry.ends_with(b"/") || !has_children).then(|| entry.to_vec())
            })
            .collect())
    }

    fn store_ignored(&self, bytes: &[u8], deadline: Instant) -> Result<Option<String>, ShadowError> {
        if bytes.len() > MAX_IGNORED_BYTES {
            return Ok(None);
        }
        let mut hash = self.git(None);
        hash.args(["hash-object", "-w", "--no-filters", "--stdin"]);
        let output = self.run_ok("hash-object", hash, Some(bytes), deadline)?;
        Ok(Some(
            String::from_utf8_lossy(git::trim_newline(&output.stdout)).into_owned(),
        ))
    }

    /// One name-only diff of every ignore file above the candidate deletion.
    pub(super) fn ancestor_ignores_changed(&self, from: &str, to: &str, path: &Path) -> Result<bool, ShadowError> {
        let mut cmd = self.git(None);
        cmd.args([
            "diff",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "-z",
            from,
            to,
            "--",
        ]);
        for dir in path.ancestors().skip(1) {
            let mut spec = OsString::from(":(top,literal)");
            spec.push(dir.join(".gitignore"));
            cmd.arg(spec);
        }
        Ok(!self
            .run_ok("diff", cmd, None, Instant::now() + READ_TIMEOUT)?
            .stdout
            .is_empty())
    }

    /// Evaluates the base's ancestor rules without consulting the live work tree's ignore files.
    /// The caller resolves external excludes once and checks their digest before using this instance.
    pub(super) fn ignored_in_base(&self, base: &str, path: &Path) -> Result<bool, ShadowError> {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let scratch = Scratch(
            self.review_dir
                .join(format!("ignore-{}", uuid::Uuid::new_v4().simple())),
        );
        agent::private_fs::create_private_dir_all(&scratch.0, private_root(&self.review_dir))?;
        for dir in path.ancestors().skip(1) {
            agent::private_fs::create_private_dir_all(&scratch.0.join(dir), &scratch.0)?;
            let ignore = dir.join(".gitignore");
            if let Some(entry) = self.read_entry(base, &ignore)? {
                if entry.kind != EntryKind::Blob {
                    return Err(ShadowError::InvalidPath(ignore));
                }
                agent::private_fs::write_private(&scratch.0.join(ignore), &entry.bytes)?;
            }
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut cmd = git::command(&self.git_dir, &scratch.0, None, self.excludes_file.as_deref());
        cmd.args(["check-ignore", "--no-index", "-q", "--"]).arg(path);
        let output = git::run(cmd, remaining(deadline)?)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => {
                GitError::check("check-ignore", output)?;
                Ok(false)
            }
        }
    }

    /// Only a tree proves that an ancestor was a directory, rather than a link or gitlink.
    pub(super) fn tree_contains(&self, commit: &str, path: &Path) -> Result<bool, ShadowError> {
        let mut spec = OsString::from(":(top,literal)");
        spec.push(path);
        let mut cmd = self.git(None);
        cmd.args(["ls-tree", "-z", "--full-name", commit, "--"]).arg(spec);
        Ok(self
            .run_ok("ls-tree", cmd, None, Instant::now() + READ_TIMEOUT)?
            .stdout
            .starts_with(b"040000 "))
    }

    /// The project's `info/exclude`, when the work tree is the top of a git work tree.
    fn project_exclude_file(&self, deadline: Instant) -> Result<Option<PathBuf>, ShadowError> {
        let mut cmd = git::user_read_only(&self.work_tree);
        cmd.args([
            "rev-parse",
            "--is-inside-work-tree",
            "--show-prefix",
            "--git-common-dir",
        ]);
        let output = match git::run(cmd, remaining(deadline)?) {
            Ok(output) if output.status.success() => output,
            Ok(_) => return Ok(None),
            Err(GitError::TimedOut) => return Err(ShadowError::TimedOut),
            Err(GitError::Spawn(e)) => return Err(ShadowError::Git(GitError::Spawn(e))),
            Err(_) => return Ok(None),
        };
        Ok(common_dir_at_top(&output.stdout).map(|common| self.work_tree.join(common).join("info/exclude")))
    }

    /// Takes a snapshot of the work tree for `session` under `label`, within `limits`. Never
    /// returns a partial snapshot: anything that goes wrong is [`SnapshotOutcome::Unavailable`]
    /// with the reason.
    pub fn snapshot(&self, session: &str, label: &SnapshotLabel, limits: &Limits) -> SnapshotOutcome {
        let deadline = Instant::now() + limits.timeout;
        match self
            .for_listing(deadline)
            .and_then(|snapshot| snapshot.snapshot_until(session, label, limits, deadline))
        {
            Ok(snapshot) => SnapshotOutcome::Taken(snapshot),
            Err(e) => SnapshotOutcome::Unavailable(e.to_string()),
        }
    }

    pub(super) fn for_listing(&self, deadline: Instant) -> Result<Self, ShadowError> {
        let mut listing = self.clone();
        if listing.user_excludes {
            listing.excludes_file = git::resolve_excludes_file_with_timeout(
                std::env::var_os("HOME").as_deref(),
                std::env::var_os("XDG_CONFIG_HOME").as_deref(),
                remaining(deadline)?,
            )?;
        }
        Ok(listing)
    }

    fn snapshot_until(
        &self,
        session: &str,
        label: &SnapshotLabel,
        limits: &Limits,
        deadline: Instant,
    ) -> Result<Snapshot, ShadowError> {
        let index = self.index_path(session)?;
        remaining(deadline)?;
        let _lock = self.lock_shared_until(deadline)?;
        let _session = self.lock_session_until(session, deadline)?;
        // Holding the session's guard, a lock on its index is one a killed run of this session
        // left; left in place, it would fail every later snapshot of the session.
        remove_if_present(&lock_file_of(&index))?;
        self.sync_excludes_until(deadline)?;
        let excludes = self.exclude_digest().ok();
        let mut ignored = self.list_ignored(&index, deadline)?;
        let (tree, skipped) = match self.write_tree(&index, limits, deadline) {
            Ok(done) => done,
            // An index can point at objects a collection has since removed (a snapshot that never
            // got its ref, an hour or more ago); starting from an empty index rebuilds it.
            Err(ShadowError::Git(GitError::Failed { what, .. })) if what == "write-tree" => {
                remove_if_present(&index)?;
                self.write_tree(&index, limits, deadline)?
            }
            Err(e) => return Err(e),
        };
        // The updated index has had ignored cached entries removed. Keep both observations:
        // an in-tree ignore rule changing during write_tree must not erase earlier evidence.
        ignored.extend(self.list_ignored(&index, deadline)?);
        let ignored: Vec<u8> = ignored
            .into_iter()
            .flat_map(|entry| entry.into_iter().chain([0]))
            .collect();
        let ignored = self.store_ignored(&ignored, deadline)?;
        let mut commit = self.git(None);
        commit.args(["commit-tree", &tree, "-F", "-"]);
        let mut message = snapshot_message(label, wall_clock_ms(), &skipped);
        if let Some(blob) = &ignored {
            message.push_str(&format!("delete-ignored: {blob}\n"));
        }
        // A rule changing while git lists files leaves no safe deletion provenance.
        if let Some(excludes) = excludes.filter(|before| self.exclude_digest().ok().as_ref() == Some(before)) {
            let taken_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            message.push_str(&format!("delete-provenance: {taken_ns} {excludes}\n"));
        }
        let commit = self.run_ok("commit-tree", commit, Some(message.as_bytes()), deadline)?;
        let commit = String::from_utf8_lossy(git::trim_newline(&commit.stdout)).into_owned();
        let reference = format!("refs/eitri/{session}/{}/{}", label.turn, label.kind.as_str());
        // Only this session's snapshots, under its guard, and a collection, under the exclusive
        // store lock, write this ref, so a lock on it now is stale too.
        remove_if_present(&lock_file_of(&self.git_dir.join(&reference)))?;
        // A hash in the commit message alone does not retain the blob through garbage collection.
        // This sibling ref is removed with the turn, and published atomically with the snapshot.
        let ignored_ref = format!("{reference}-ignored");
        remove_if_present(&lock_file_of(&self.git_dir.join(&ignored_ref)))?;
        let mut updates = format!("update {reference} {commit}\n");
        if let Some(blob) = ignored {
            updates.push_str(&format!("update {ignored_ref} {blob}\n"));
        } else {
            updates.push_str(&format!("delete {ignored_ref}\n"));
        }
        let mut update = self.git(None);
        update.args(["update-ref", "--stdin"]);
        self.run_ok("update-ref", update, Some(updates.as_bytes()), deadline)?;
        Ok(Snapshot {
            commit,
            tree,
            reference,
            skipped_large: skipped.iter().map(|(path, _)| path.clone()).collect(),
            left_out: skipped.into_iter().collect(),
        })
    }

    /// Records `marks` for turn `turn` of `session` (provider turn id `turn_id`), replacing what was
    /// recorded before: a commit of the empty tree whose message holds them, under
    /// `refs/eitri/<session>/<turn>/marks`. Retention removes it with the turn's snapshots.
    pub fn write_marks(&self, session: &str, turn: u32, turn_id: &str, marks: TurnMarks) -> Result<(), ShadowError> {
        let deadline = Instant::now() + READ_TIMEOUT;
        validate_session(session)?;
        let _lock = self.lock_shared_until(deadline)?;
        let _session = self.lock_session_until(session, deadline)?;
        let mut empty = self.git(None);
        empty.arg("mktree");
        let tree = self.run_ok("mktree", empty, Some(b""), deadline)?;
        let tree = String::from_utf8_lossy(git::trim_newline(&tree.stdout)).into_owned();
        let mut commit = self.git(None);
        commit.args(["commit-tree", &tree, "-F", "-"]);
        let message = marks.to_message(turn, turn_id);
        let commit = self.run_ok("commit-tree", commit, Some(message.as_bytes()), deadline)?;
        let commit = String::from_utf8_lossy(git::trim_newline(&commit.stdout)).into_owned();
        let reference = format!("refs/eitri/{session}/{turn}/{MARKS}");
        // Written only under the session's guard, like its snapshot refs, so a lock on it is stale.
        remove_if_present(&lock_file_of(&self.git_dir.join(&reference)))?;
        let mut update = self.git(None);
        update.args(["update-ref", &reference, &commit]);
        self.run_ok("update-ref", update, None, deadline)?;
        Ok(())
    }

    /// The tree of the work tree as it is now, written to the store without a commit or a ref: what
    /// a comparison against the live files is made with. It goes through the same isolated
    /// indexing as [`snapshot`](Self::snapshot) (so the same files are left out for size or
    /// ignore rules), but into a throwaway index, never the live one and never a session's.
    /// Returns the tree id and the paths and prints left out for size.
    ///
    /// Nothing references the returned tree; it stays readable until a collection prunes it, so a
    /// caller that holds [`lock_shared`](Self::lock_shared) from before this call until its last
    /// read of the tree is safe from that.
    pub fn scratch_tree(&self, limits: &Limits) -> Result<(String, Vec<(PathBuf, Option<FilePrint>)>), ShadowError> {
        let deadline = Instant::now() + limits.timeout;
        self.for_listing(deadline)?.scratch_tree_until(limits, deadline)
    }

    fn scratch_tree_until(
        &self,
        limits: &Limits,
        deadline: Instant,
    ) -> Result<(String, Vec<(PathBuf, Option<FilePrint>)>), ShadowError> {
        let session = format!("scratch-{}", uuid::Uuid::new_v4().simple());
        let index = self.index_path(&session)?;
        let _lock = self.lock_shared_until(deadline)?;
        self.sync_excludes_until(deadline)?;
        let written = self.write_tree(&index, limits, deadline);
        let _ = remove_if_present(&index);
        // A run killed for its time leaves the lock; nothing else will ever use this index.
        let _ = remove_if_present(&lock_file_of(&index));
        written
    }

    /// Brings `index` up to date with the work tree and writes its tree, returning the tree and the
    /// files left out for size. The caller holds the lock.
    fn write_tree(
        &self,
        index: &Path,
        limits: &Limits,
        deadline: Instant,
    ) -> Result<(String, Vec<(PathBuf, Option<FilePrint>)>), ShadowError> {
        let mut list = self.git(Some(index));
        list.args(["ls-files", "-z", "--others", "--cached", "--exclude-standard"]);
        let listed = self.run_ok("ls-files", list, None, deadline)?;
        let mut ignored = self.git(Some(index));
        ignored.args(["ls-files", "-z", "--cached", "--ignored", "--exclude-standard"]);
        let ignored = self.run_ok("ls-files", ignored, None, deadline)?;
        let ignored_paths: BTreeSet<&[u8]> = nul_separated(&ignored.stdout).collect();
        // Cached entries include paths deleted or ignored since the previous snapshot.
        let entries: BTreeSet<&[u8]> = nul_separated(&listed.stdout)
            .filter(|entry| !ignored_paths.contains(entry))
            .collect();
        #[cfg(test)]
        tests::AFTER_LISTING.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let mut count = 0;
        let mut large: Vec<&[u8]> = Vec::new();
        let mut prints: Vec<FilePrint> = Vec::new();
        let mut left_out: Vec<&[u8]> = Vec::new();
        for entry in &entries {
            remaining(deadline)?;
            let path = self.work_tree.join(OsStr::from_bytes(entry));
            let Ok(meta) = path.symlink_metadata() else { continue };
            count += 1;
            if let Some(dir) = entry.strip_suffix(b"/") {
                // An untracked nested repository: one gitlink entry, unless it has no commit yet,
                // which git refuses to add at all.
                if !self.nested_has_commit(dir, deadline)? {
                    left_out.push(dir);
                }
                continue;
            }
            if meta.is_file() && meta.len() > limits.max_file_bytes {
                large.push(entry);
                prints.push(FilePrint::of(&meta));
            }
        }
        if count > limits.max_files {
            return Err(ShadowError::TooManyFiles {
                count,
                max: limits.max_files,
            });
        }

        if !large.is_empty() {
            let mut remove = self.git(Some(index));
            remove.args(["update-index", "-z", "--force-remove", "--stdin"]);
            self.run_ok("update-index", remove, Some(&nul_joined(&large)), deadline)?;
        }

        if !ignored.stdout.is_empty() {
            let mut remove = self.git(Some(index));
            remove.args(["update-index", "-z", "--force-remove", "--stdin"]);
            self.run_ok("update-index", remove, Some(&ignored.stdout), deadline)?;
        }

        // What the index holds before the add, so that what the add changed can be told apart. The
        // listing above is a moment older than the add: a file made or grown in between reaches the
        // add without its size having been checked, and once in the index unchanged, no later
        // listing names it again.
        let before = self.index_blobs(index, deadline)?;
        let added_and_checked = || -> Result<Vec<Vec<u8>>, ShadowError> {
            let mut add = self.git(Some(index));
            add.args(["add", "-A"]);
            let exclusions: Vec<u8> = large
                .iter()
                .chain(left_out.iter())
                .flat_map(|p| b":(top,exclude,literal)".iter().chain(p.iter()).chain(b"\0".iter()))
                .copied()
                .collect();
            if exclusions.is_empty() {
                self.run_ok("add", add, None, deadline)?;
            } else {
                add.args(["--pathspec-from-file=-", "--pathspec-file-nul"]);
                self.run_ok("add", add, Some(&exclusions), deadline)?;
            }
            #[cfg(test)]
            if let Some(hook) = tests::AFTER_ADD.with(|hook| hook.borrow_mut().take()) {
                hook()?;
            }

            // So every blob the add stored or changed is measured as stored, and one over the limit
            // leaves the index too: a snapshot never holds what it would have left out had the file
            // been that size when listed.
            let after = self.index_blobs(index, deadline)?;
            let changed: Vec<(&[u8], &str)> = after
                .iter()
                .filter(|(path, blob)| before.get(*path) != Some(*blob))
                .map(|(path, blob)| (path.as_slice(), blob.as_str()))
                .collect();
            let grown = self.blobs_over(&changed, limits.max_file_bytes, deadline)?;
            if !grown.is_empty() {
                let mut remove = self.git(Some(index));
                remove.args(["update-index", "-z", "--force-remove", "--stdin"]);
                self.run_ok("update-index", remove, Some(&nul_joined(&grown)), deadline)?;
            }
            Ok(grown.into_iter().map(<[u8]>::to_vec).collect())
        };
        let grown = match added_and_checked() {
            Ok(grown) => grown,
            Err(e) => {
                // The add may have stored a blob that the check never measured or removed. Kept, the
                // next snapshot would find it unchanged and hold it at any size; that one starts from an
                // empty index instead, which measures every blob again.
                let _ = remove_if_present(index);
                return Err(e);
            }
        };

        let mut write = self.git(Some(index));
        write.arg("write-tree");
        let tree = self.run_ok("write-tree", write, None, deadline)?;
        let tree = String::from_utf8_lossy(git::trim_newline(&tree.stdout)).into_owned();
        let mut skipped: Vec<(PathBuf, Option<FilePrint>)> = large
            .iter()
            .zip(prints)
            .map(|(p, print)| (PathBuf::from(OsString::from_vec(p.to_vec())), Some(print)))
            .collect();
        for path in grown {
            let path = PathBuf::from(OsString::from_vec(path));
            // The print of the file as it is now: what the listing would have taken had it been
            // this size then. A file gone or no longer regular has none, so its change is not ruled out.
            let print = FilePrint::of_path(&self.work_tree, &path);
            skipped.push((path, print));
        }
        Ok((tree, skipped))
    }

    /// The regular files (plain or executable) in `index`, each with its blob id.
    fn index_blobs(&self, index: &Path, deadline: Instant) -> Result<BTreeMap<Vec<u8>, String>, ShadowError> {
        let mut list = self.git(Some(index));
        list.args(["ls-files", "-z", "--stage"]);
        let listed = self.run_ok("ls-files", list, None, deadline)?;
        let mut blobs = BTreeMap::new();
        for entry in nul_separated(&listed.stdout) {
            // `<mode> <blob> <stage>\t<path>`; the path may hold any byte but NUL.
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let (meta, path) = (&entry[..tab], &entry[tab + 1..]);
            let mut fields = meta.split(|&b| b == b' ');
            let (Some(mode), Some(blob)) = (fields.next(), fields.next()) else {
                continue;
            };
            if mode == b"100644" || mode == b"100755" {
                blobs.insert(path.to_vec(), String::from_utf8_lossy(blob).into_owned());
            }
        }
        Ok(blobs)
    }

    /// The paths among `entries` whose blob is larger than `max` bytes.
    fn blobs_over<'a>(
        &self,
        entries: &[(&'a [u8], &str)],
        max: u64,
        deadline: Instant,
    ) -> Result<Vec<&'a [u8]>, ShadowError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: BTreeSet<&str> = entries.iter().map(|(_, blob)| *blob).collect();
        let mut input = String::new();
        for blob in &wanted {
            input.push_str(blob);
            input.push('\n');
        }
        let mut check = self.git(None);
        check.args(["cat-file", "--batch-check=%(objectname) %(objectsize)"]);
        let checked = self.run_ok("cat-file", check, Some(input.as_bytes()), deadline)?;
        let mut over = BTreeSet::new();
        for line in String::from_utf8_lossy(&checked.stdout).lines() {
            match line.split_once(' ') {
                Some((blob, size)) => match size.parse::<u64>() {
                    Ok(size) if size <= max => {}
                    // Larger, or not a size: a blob the add just stored and git cannot measure is
                    // not kept on trust.
                    _ => {
                        over.insert(blob.to_owned());
                    }
                },
                None => {
                    over.insert(line.to_owned());
                }
            }
        }
        Ok(entries
            .iter()
            .filter(|(_, blob)| over.contains(*blob))
            .map(|(path, _)| *path)
            .collect())
    }

    /// Whether the nested repository at `dir` (relative to the work tree) has a commit checked out.
    /// Read-only, and isolated like every other shadow git run, except that git reads that
    /// repository's own config (the `git` module doc says why that is harmless).
    fn nested_has_commit(&self, dir: &[u8], deadline: Instant) -> Result<bool, ShadowError> {
        let nested = self.work_tree.join(OsStr::from_bytes(dir));
        let mut cmd = git::command(&nested.join(".git"), &nested, None, None);
        cmd.args(["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
        Ok(git::run(cmd, remaining(deadline)?)?.status.success())
    }

    /// The bytes of `path` (relative to the work tree) in snapshot `commit`, exactly as they were on
    /// disk; `None` when the snapshot has no file there.
    pub fn read_blob(&self, commit: &str, path: &Path) -> Result<Option<Vec<u8>>, ShadowError> {
        if !is_hex_id(commit) {
            return Err(ShadowError::NoSuchCommit(commit.to_owned()));
        }
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err(ShadowError::InvalidPath(path.to_path_buf()));
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        let mut spec = OsString::from(format!("{commit}:"));
        spec.push(path.as_os_str());

        let mut kind = self.git(None);
        kind.args(["cat-file", "-t"]).arg(&spec);
        let kind = git::run(kind, remaining(deadline)?)?;
        if !kind.status.success() {
            let mut verify = self.git(None);
            verify.args(["rev-parse", "--verify", "-q", &format!("{commit}^{{commit}}")]);
            return if git::run(verify, remaining(deadline)?)?.status.success() {
                Ok(None)
            } else {
                Err(ShadowError::NoSuchCommit(commit.to_owned()))
            };
        }
        if git::trim_newline(&kind.stdout) != b"blob" {
            return Ok(None);
        }
        let mut cat = self.git(None);
        cat.args(["cat-file", "blob"]).arg(&spec);
        Ok(Some(self.run_ok("cat-file", cat, None, deadline)?.stdout))
    }

    /// Stores `bytes` as a blob, exactly as given (no filter or conversion runs over them), and
    /// returns its id. Nothing refers to it yet: a collection may prune it once it is older than its
    /// prune expiry, so a caller that needs it longer anchors it ([`anchor_revert`](Self::anchor_revert)).
    pub fn store_blob(&self, bytes: &[u8]) -> Result<String, ShadowError> {
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        let mut hash = self.git(None);
        hash.args(["hash-object", "-w", "--no-filters", "--stdin"]);
        let output = self.run_ok("hash-object", hash, Some(bytes), deadline)?;
        Ok(String::from_utf8_lossy(git::trim_newline(&output.stdout)).into_owned())
    }

    /// The bytes of the stored blob `blob`; `None` when the store has no blob of that id. Only a
    /// full id is accepted: an abbreviation that is unique today may name two objects tomorrow.
    pub fn read_stored(&self, blob: &str) -> Result<Option<Vec<u8>>, ShadowError> {
        if !is_full_id(blob) {
            return Err(ShadowError::InvalidName(blob.to_owned()));
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        let mut kind = self.git(None);
        kind.args(["cat-file", "-t", blob]);
        let kind = git::run(kind, remaining(deadline)?)?;
        if !kind.status.success() || git::trim_newline(&kind.stdout) != b"blob" {
            return Ok(None);
        }
        let mut cat = self.git(None);
        cat.args(["cat-file", "blob", blob]);
        Ok(Some(self.run_ok("cat-file", cat, None, deadline)?.stdout))
    }

    /// The stored object's length, without loading its contents; missing objects have no length.
    pub(super) fn stored_len(&self, blob: &str) -> Result<Option<u64>, ShadowError> {
        if !is_full_id(blob) {
            return Err(ShadowError::InvalidName(blob.to_owned()));
        }
        let mut cmd = self.git(None);
        cmd.args(["cat-file", "--batch-check=%(objectsize)"]);
        let input = format!("{blob}\n");
        let output = self.run_ok("cat-file", cmd, Some(input.as_bytes()), Instant::now() + READ_TIMEOUT)?;
        Ok(String::from_utf8_lossy(git::trim_newline(&output.stdout)).parse().ok())
    }

    /// What `path` (relative to the work tree) is in snapshot `commit`: its kind, its mode and its
    /// bytes. `None` when the snapshot has nothing there, or only a directory.
    pub fn read_entry(&self, commit: &str, path: &Path) -> Result<Option<Entry>, ShadowError> {
        Ok(self.read_entry_with_id(commit, path)?.map(|(entry, _)| entry))
    }

    /// [`read_entry`](Self::read_entry) with the id of the object the snapshot holds at `path` (a
    /// blob's, or the nested repository's commit), so a caller can name the bytes to git without
    /// storing them again.
    pub fn read_entry_with_id(&self, commit: &str, path: &Path) -> Result<Option<(Entry, String)>, ShadowError> {
        if !is_hex_id(commit) {
            return Err(ShadowError::NoSuchCommit(commit.to_owned()));
        }
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err(ShadowError::InvalidPath(path.to_path_buf()));
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        // `top` and `literal` magic: the path is the path, from the top, never a pattern.
        let mut spec = OsString::from(":(top,literal)");
        spec.push(path.as_os_str());
        let mut list = self.git(None);
        list.args(["ls-tree", "-z", "--full-name", commit, "--"]).arg(&spec);
        let listed = git::run(list, remaining(deadline)?)?;
        if !listed.status.success() {
            let mut verify = self.git(None);
            verify.args(["rev-parse", "--verify", "-q", &format!("{commit}^{{commit}}")]);
            if !git::run(verify, remaining(deadline)?)?.status.success() {
                return Err(ShadowError::NoSuchCommit(commit.to_owned()));
            }
            GitError::check("ls-tree", listed)?;
            return Ok(None);
        }
        let wanted = path.as_os_str().as_bytes();
        let Some((mode, id)) = nul_separated(&listed.stdout).find_map(|record| {
            let (meta, name) = split_once_byte(record, b'\t')?;
            if name != wanted {
                return None;
            }
            let mut fields = meta.split(|&b| b == b' ');
            let (mode, _kind, id) = (fields.next()?, fields.next()?, fields.next()?);
            Some((mode.to_vec(), String::from_utf8_lossy(id).into_owned()))
        }) else {
            return Ok(None);
        };
        let (kind, mode) = match mode.as_slice() {
            b"120000" => (EntryKind::Symlink, 0o777),
            b"160000" => {
                return Ok(Some((
                    Entry {
                        mode: 0,
                        kind: EntryKind::Gitlink,
                        bytes: id.clone().into_bytes(),
                    },
                    id,
                )))
            }
            b"100755" => (EntryKind::Blob, 0o755),
            m if m.starts_with(b"100") => (EntryKind::Blob, 0o644),
            _ => return Ok(None),
        };
        let mut cat = self.git(None);
        cat.args(["cat-file", "blob", &id]);
        let bytes = self.run_ok("cat-file", cat, None, deadline)?.stdout;
        Ok(Some((Entry { mode, kind, bytes }, id)))
    }

    /// Keeps the blobs a revert wrote over (`pre`) and wrote (`post`) for as long as retention keeps
    /// revert anchors: `refs/eitri-revert/<session>/<stamp>/{pre,post}`, `stamp` being
    /// `<ms>-<seq>`. A side given as `None` gets no ref.
    pub fn anchor_revert(
        &self,
        session: &str,
        stamp: &str,
        pre: Option<&str>,
        post: Option<&str>,
    ) -> Result<(), ShadowError> {
        validate_session(session)?;
        if parse_stamp_ms(stamp).is_none() {
            return Err(ShadowError::InvalidName(stamp.to_owned()));
        }
        let prefix = format!("refs/eitri-revert/{session}/{stamp}");
        let input = ref_lines(
            "update",
            [(format!("{prefix}/pre"), pre), (format!("{prefix}/post"), post)],
        )?;
        if input.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        let mut update = self.git(None);
        update.args(["update-ref", "--stdin"]);
        self.run_ok("update-ref", update, Some(input.as_bytes()), deadline)?;
        Ok(())
    }

    /// Pins a journal entry's blobs under `refs/eitri-journal/<id>/{pre,intended}`, which retention
    /// never ages out. `create` refuses a ref that exists and an object that does not, so a pin
    /// that was made proves its blob is in the store. The caller holds the store's lock.
    pub(crate) fn pin_journal_locked(
        &self,
        id: &str,
        pre: Option<&str>,
        intended: Option<&str>,
    ) -> Result<(), ShadowError> {
        if !is_journal_id(id) {
            return Err(ShadowError::InvalidName(id.to_owned()));
        }
        let prefix = format!("refs/eitri-journal/{id}");
        let input = ref_lines(
            "create",
            [(format!("{prefix}/pre"), pre), (format!("{prefix}/intended"), intended)],
        )?;
        if input.is_empty() {
            return Ok(());
        }
        let mut update = self.git(None);
        update.args(["update-ref", "--stdin"]);
        self.run_ok(
            "update-ref",
            update,
            Some(input.as_bytes()),
            Instant::now() + READ_TIMEOUT,
        )?;
        Ok(())
    }

    /// Removes a journal entry's pins. Removing a pin that is already gone is not an error.
    pub(crate) fn unpin_journal(&self, id: &str) -> Result<(), ShadowError> {
        let _lock = self.lock_shared_until(Instant::now() + READ_TIMEOUT)?;
        self.unpin_journal_locked(id)
    }

    /// [`unpin_journal`](Self::unpin_journal) with the store's lock already held.
    pub(crate) fn unpin_journal_locked(&self, id: &str) -> Result<(), ShadowError> {
        if !is_journal_id(id) {
            return Err(ShadowError::InvalidName(id.to_owned()));
        }
        let input = format!("delete refs/eitri-journal/{id}/pre\ndelete refs/eitri-journal/{id}/intended\n");
        let mut update = self.git(None);
        update.args(["update-ref", "--stdin"]);
        self.run_ok(
            "update-ref",
            update,
            Some(input.as_bytes()),
            Instant::now() + READ_TIMEOUT,
        )?;
        Ok(())
    }

    /// Every snapshot ref, with its label (`None` when its message is not a label).
    pub fn snapshot_refs(&self) -> Result<Vec<(String, Option<SnapshotLabel>)>, ShadowError> {
        let _lock = self.lock_shared_until(Instant::now() + READ_TIMEOUT)?;
        Ok(self
            .snapshot_refs_locked(Instant::now() + READ_TIMEOUT)?
            .into_iter()
            .filter(|(name, _)| !name.ends_with("-ignored"))
            .collect())
    }

    fn snapshot_refs_locked(&self, deadline: Instant) -> Result<Vec<(String, Option<SnapshotLabel>)>, ShadowError> {
        let mut list = self.git(None);
        list.args(["for-each-ref", "--format=%(refname)%00%(contents)%00", "refs/eitri/"]);
        let output = self.run_ok("for-each-ref", list, None, deadline)?;
        let mut fields = output.stdout.split(|&b| b == 0);
        let mut refs = Vec::new();
        while let (Some(name), Some(message)) = (fields.next(), fields.next()) {
            let name = String::from_utf8_lossy(name.strip_prefix(b"\n").unwrap_or(name)).into_owned();
            if name.is_empty() {
                break;
            }
            refs.push((name, SnapshotLabel::parse(&String::from_utf8_lossy(message))));
        }
        Ok(refs)
    }

    /// Every snapshot ref with the commit it points at and its label (`None` when its message is
    /// not a label): what a review of turns taken by an earlier run is built from.
    pub fn snapshot_ref_ids(&self) -> Result<Vec<SnapshotRef>, ShadowError> {
        let deadline = Instant::now() + READ_TIMEOUT;
        let _lock = self.lock_shared_until(deadline)?;
        let mut list = self.git(None);
        list.args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(contents)%00",
            "refs/eitri/",
        ]);
        let output = self.run_ok("for-each-ref", list, None, deadline)?;
        let mut fields = output.stdout.split(|&b| b == 0);
        let mut refs = Vec::new();
        while let (Some(name), Some(commit), Some(message)) = (fields.next(), fields.next(), fields.next()) {
            let name = String::from_utf8_lossy(name.strip_prefix(b"\n").unwrap_or(name)).into_owned();
            if name.is_empty() {
                break;
            }
            let message = String::from_utf8_lossy(message);
            if name.ends_with("-ignored") {
                continue;
            }
            refs.push(SnapshotRef {
                name,
                commit: String::from_utf8_lossy(commit).into_owned(),
                label: SnapshotLabel::parse(&message),
                taken_ms: parse_taken(&message),
                skipped_large: parse_skipped_large(&message),
                left_out: parse_left_out(&message),
                marks: TurnMarks::parse(&message),
            });
        }
        Ok(refs)
    }

    /// Deletes the snapshots of every turn beyond the newest `keep` turns of the project, and of
    /// every turn older than `max_age` at `now_ms` (by the time in its labels), then collects
    /// garbage, removing unreachable objects older than `prune_expiry` (a git date: production
    /// passes `1.hour.ago`, so an object a concurrent snapshot wrote moments ago is never removed
    /// before its ref exists). An index whose session has no snapshot left is removed too, so it
    /// cannot point at objects the collection frees. Revert anchors (`refs/eitri-revert/`) older
    /// than `max_age` by their stamp go too, whatever `keep` says; a journal entry's pins
    /// (`refs/eitri-journal/`) are never aged out and go only once their entry is gone.
    ///
    /// A snapshot never waits for the slow part. The work is done in three steps:
    /// 1. With the lock held exclusively: the refs above are deleted and the indexes removed.
    /// 2. With the lock held shared, so snapshots, reads and writes go on beside it: `git gc` that
    ///    deletes nothing ([`collect_keeping_everything`](Self::collect_keeping_everything)).
    /// 3. With the lock held exclusively again: `git prune` removes the loose objects no ref
    ///    reaches that are older than `prune_expiry`, and the refs are packed.
    ///
    /// Objects and refs are deleted only in steps 1 and 3, when no other git run of any window is
    /// using the store, so nothing is removed that a snapshot has written or brought back but not
    /// yet referenced, exactly as when the whole collection held the lock exclusively.
    ///
    /// Only one collection of the project runs at a time: one that finds another window's running
    /// waits for it on `collect.lock`, so two `git gc` never meet in the store.
    pub fn trim(&self, keep: usize, max_age: Duration, now_ms: u64, prune_expiry: &str) -> Result<(), ShadowError> {
        self.trim_notifying(keep, max_age, now_ms, prune_expiry, |_| {})
    }

    /// Reports removed turns as soon as their refs are deleted, even if later collection fails.
    pub(crate) fn trim_notifying(
        &self,
        keep: usize,
        max_age: Duration,
        now_ms: u64,
        prune_expiry: &str,
        removed: impl FnOnce(Vec<(String, u32)>),
    ) -> Result<(), ShadowError> {
        let _collecting = flock_file(&self.review_dir.join("collect.lock"), libc::LOCK_EX, None)?;
        // Collection is not bounded by the snapshot timeout: it runs in the background, and
        // stopping it halfway only wastes the work.
        let deadline = Instant::now() + Duration::from_secs(600);
        self.drop_expired(keep, max_age, now_ms, deadline, removed)?;
        {
            let _lock = self.lock_shared()?;
            #[cfg(test)]
            super::test_hooks::run(&super::test_hooks::BEFORE_GC);
            self.collect_keeping_everything(deadline)?;
        }
        #[cfg(test)]
        super::test_hooks::run(&super::test_hooks::BEFORE_PRUNE);
        let _lock = self.lock_exclusive()?;
        let mut prune = self.git(None);
        prune.arg("prune").arg(format!("--expire={prune_expiry}"));
        self.run_ok("prune", prune, None, deadline)?;
        let mut pack = self.git(None);
        pack.args(["pack-refs", "--all", "--prune"]);
        self.run_ok("pack-refs", pack, None, deadline)?;
        Ok(())
    }

    /// `git gc` that deletes no object and writes no ref, so it can run while snapshots write
    /// objects and refs; the caller holds the lock shared. What it does is repack: every object a
    /// ref reaches goes into one new pack, and every other object of an old pack is written out as
    /// a loose object (`repack -A`) before that pack is removed, for step 3 of
    /// [`trim`](Self::trim) to judge by its age.
    ///
    /// - `--no-prune`: neither the repack nor a `git prune` removes an unreachable object. One a
    ///   snapshot writes or brings back while this runs is still there afterwards, loose or
    ///   packed, whatever this run decided about it.
    /// - `gc.cruftPacks=false`: unreachable objects are written out loose, which `git prune` can
    ///   remove one by one; in a cruft pack they could only go by repacking again.
    /// - `gc.packRefs=false`: packing refs locks each loose ref it removes, and a snapshot removes
    ///   a lock it finds on the ref it is about to write (only a killed run leaves one, when
    ///   nothing else writes that ref). Refs are packed in step 3 instead.
    ///
    /// Its own lock files (`gc.pid.lock`, the commit graph's) are safe from the stale-lock sweep,
    /// which runs only with the lock held exclusively.
    fn collect_keeping_everything(&self, deadline: Instant) -> Result<(), ShadowError> {
        let mut gc = self.git(None);
        gc.args(["-c", "gc.cruftPacks=false", "-c", "gc.packRefs=false"])
            .args(["gc", "--no-prune", "--quiet"]);
        self.run_ok("gc", gc, None, deadline)?;
        Ok(())
    }

    /// Step 1 of [`trim`](Self::trim): the expired refs and the indexes of sessions with nothing
    /// left, with the lock held exclusively.
    fn drop_expired(
        &self,
        keep: usize,
        max_age: Duration,
        now_ms: u64,
        deadline: Instant,
        removed: impl FnOnce(Vec<(String, u32)>),
    ) -> Result<(), ShadowError> {
        let _lock = self.lock_exclusive()?;
        // A ref lock or `packed-refs.lock` left by a killed snapshot or collection would make the
        // ref deletion below fail, and with it every collection from then on.
        self.clear_stale_locks()?;
        let refs = self.snapshot_refs_locked(deadline)?;

        // Turn -> (newest label time, its refs).
        let mut turns: BTreeMap<(String, u32), (u64, Vec<String>)> = BTreeMap::new();
        let mut latest = BTreeMap::<String, u32>::new();
        for (name, label) in &refs {
            let Some(rest) = name.strip_prefix("refs/eitri/") else {
                continue;
            };
            let mut parts = rest.split('/');
            let (Some(session), Some(turn)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Ok(turn) = turn.parse::<u32>() else { continue };
            latest
                .entry(session.to_owned())
                .and_modify(|last| *last = (*last).max(turn))
                .or_insert(turn);
            let entry = turns.entry((session.to_owned(), turn)).or_default();
            entry.0 = entry.0.max(label.as_ref().map_or(0, |l| l.time_ms));
            entry.1.push(name.clone());
        }
        let mut ordered: Vec<_> = turns.into_iter().collect();
        ordered.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| b.0.cmp(&a.0)));
        let oldest_kept = now_ms.saturating_sub(u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX));

        let mut delete = Vec::new();
        let mut removed_turns = Vec::new();
        let mut kept_sessions = BTreeSet::new();
        let mut finished = 0;
        for ((session, turn), (time, names)) in ordered {
            let running = latest.get(&session) == Some(&turn)
                && names.iter().any(|name| name.ends_with("/base"))
                && !names.iter().any(|name| name.ends_with("/end"));
            let over_count = !running && finished >= keep;
            if !running {
                finished += 1;
            }
            if over_count || time < oldest_kept {
                removed_turns.push((session, turn));
                delete.extend(names);
            } else {
                kept_sessions.insert(session);
            }
        }
        delete.extend(self.expired_revert_refs_locked(oldest_kept, deadline)?);
        if !delete.is_empty() {
            let input: String = delete.iter().map(|name| format!("delete {name}\n")).collect();
            let mut update = self.git(None);
            update.args(["update-ref", "--stdin"]);
            self.run_ok("update-ref", update, Some(input.as_bytes()), deadline)?;
        }
        removed(removed_turns);

        for entry in std::fs::read_dir(&self.review_dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix("index-") else {
                continue;
            };
            // The index and its guard go together; no snapshot holds a guard while the store
            // lock is held exclusively.
            let session = rest.strip_suffix(".guard").unwrap_or(rest);
            if validate_session(session).is_err() {
                continue;
            }
            if !kept_sessions.contains(session) {
                remove_if_present(&self.review_dir.join(name))?;
            }
        }
        Ok(())
    }

    /// The revert anchors stamped before `oldest_kept`, and the journal pins whose entry no longer
    /// exists. A journal pin is never judged by age: an interrupted in-place write's bytes must stay
    /// for as long as its entry waits to be restored or dismissed, however long that is. The store
    /// lock is held exclusively, so no entry is between being pinned and being written: a missing
    /// entry is one that was removed, or whose writer died before it existed.
    fn expired_revert_refs_locked(&self, oldest_kept: u64, deadline: Instant) -> Result<Vec<String>, ShadowError> {
        let mut list = self.git(None);
        list.args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/eitri-revert/",
            "refs/eitri-journal/",
        ]);
        let output = self.run_ok("for-each-ref", list, None, deadline)?;
        let journal = self.review_dir.join("journal");
        let mut expired = Vec::new();
        for name in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<&str> = name.split('/').collect();
            match parts.as_slice() {
                ["refs", "eitri-revert", _session, stamp, _side] => {
                    // A stamp that does not parse is kept: nothing here says how old it is.
                    if parse_stamp_ms(stamp).is_some_and(|ms| ms < oldest_kept) {
                        expired.push(name.to_owned());
                    }
                }
                ["refs", "eitri-journal", id, _side] if is_journal_id(id) => {
                    match std::fs::symlink_metadata(journal.join(format!("{id}.json"))) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => expired.push(name.to_owned()),
                        // Anything else, the entry itself included, keeps the pin.
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(expired)
    }
}

/// Takes an `flock` (`operation`) on `path`, creating it 0600. With a `deadline` it polls and gives
/// up with [`ShadowError::TimedOut`] there; without one it waits as long as it takes.
pub(crate) fn flock_file(
    path: &Path,
    operation: libc::c_int,
    deadline: Option<Instant>,
) -> Result<ShadowLock, ShadowError> {
    if let Some(deadline) = deadline {
        remaining(deadline)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(agent::private_fs::PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // Exactly 0600 whatever the umask: one that takes the owner's write bit away would leave a 0400
    // file, which another window's probe (it opens the file for writing) could not open, and every
    // revert would be refused.
    file.set_permissions(std::fs::Permissions::from_mode(agent::private_fs::PRIVATE_FILE_MODE))?;
    let flags = if deadline.is_some() {
        operation | libc::LOCK_NB
    } else {
        operation
    };
    loop {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(ShadowLock { file });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) => match deadline {
                Some(deadline) => {
                    remaining(deadline)?;
                    std::thread::sleep(Duration::from_millis(2));
                }
                None => return Err(err.into()),
            },
            _ => return Err(err.into()),
        }
    }
}

/// What opening a socket fails with: `ENXIO` on Linux, `EOPNOTSUPP` on macOS. Each OS's own only, so
/// nothing else is ever taken for a socket.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SOCKET_OPEN: i32 = libc::ENXIO;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SOCKET_OPEN: i32 = libc::EOPNOTSUPP;

/// Tries to take `path`'s `flock` exclusively without creating it: `Some` when it was free (the
/// caller now holds it), `None` when someone else holds it. A path that is missing, a symlink or
/// not a regular file (a directory, a socket, a FIFO) is [`std::io::ErrorKind::NotFound`]: there
/// is nothing there to lock, and a FIFO someone planted cannot hold the open up.
///
/// The file is opened for writing as well, though nothing is written: on NFS Linux takes an
/// `flock` as a byte-range lock, and an exclusive one on a descriptor open only for reading fails
/// with `EBADF`, which would make every presence check fail on a home directory served over NFS.
pub(crate) fn try_flock_existing(path: &Path) -> Result<Option<ShadowLock>, ShadowError> {
    let not_a_lock_file = || ShadowError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "not a lock file"));
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    {
        Ok(file) => file,
        // `O_NOFOLLOW` on a symlink; a directory, which cannot be opened for writing; a socket,
        // which cannot be opened at all (`SOCKET_OPEN`).
        Err(e)
            if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::EISDIR))
                || e.raw_os_error() == Some(SOCKET_OPEN) =>
        {
            return Err(not_a_lock_file())
        }
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(not_a_lock_file());
    }
    loop {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(ShadowLock { file }));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) => return Ok(None),
            _ => return Err(err.into()),
        }
    }
}

/// A session id is a file-name and ref component: ASCII letters, digits, `-` and `_`, at most 128.
pub(crate) fn validate_session(session: &str) -> Result<(), ShadowError> {
    let ok = !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ShadowError::InvalidSession(session.to_owned()))
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn is_hex_id(id: &str) -> bool {
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A whole object id, SHA-1 or SHA-256, in lower-case hex as git prints it.
pub(super) fn is_full_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A journal entry's id: a simple UUID, 32 lower-case hex digits.
pub(super) fn is_journal_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The milliseconds of a revert stamp `<ms>-<seq>`, when it has that shape.
fn parse_stamp_ms(stamp: &str) -> Option<u64> {
    let (ms, seq) = stamp.split_once('-')?;
    let digits = |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(ms, 20) || !digits(seq, 10) {
        return None;
    }
    ms.parse().ok()
}

/// `update-ref --stdin` lines (`verb` is `update` or `create`) giving each named ref that has a
/// blob that blob.
fn ref_lines(verb: &str, refs: [(String, Option<&str>); 2]) -> Result<String, ShadowError> {
    let mut input = String::new();
    for (name, blob) in refs {
        let Some(blob) = blob else { continue };
        if !is_full_id(blob) {
            return Err(ShadowError::InvalidName(blob.to_owned()));
        }
        input.push_str(&format!("{verb} {name} {blob}\n"));
    }
    Ok(input)
}

fn split_once_byte(bytes: &[u8], at: u8) -> Option<(&[u8], &[u8])> {
    let i = bytes.iter().position(|&b| b == at)?;
    Some((&bytes[..i], &bytes[i + 1..]))
}

pub(super) fn remaining(deadline: Instant) -> Result<Duration, ShadowError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(ShadowError::TimedOut)
    } else {
        Ok(left)
    }
}

fn nul_separated(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == 0).filter(|s| !s.is_empty())
}

fn nul_joined(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(item);
        out.push(0);
    }
    out
}

/// `<path>.lock`: the file git creates beside `path` while it rewrites it.
fn lock_file_of(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Removes every regular file named `index-*.lock` directly in `dir`: the lock git takes on a
/// session's index, which lives beside the git directory rather than in it. Nothing else of that
/// directory's is a lock git leaves.
fn remove_index_locks(dir: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.as_bytes();
        if entry.file_type()?.is_file() && name.starts_with(b"index-") && name.ends_with(b".lock") {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

/// Removes every regular file named `*.lock` in `dir`, and in its subdirectories when `recursive`.
/// A missing `dir` is fine; symlinks are neither followed nor removed.
fn remove_lock_files(dir: &Path, recursive: bool) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if recursive {
                remove_lock_files(&entry.path(), true)?;
            }
        } else if kind.is_file() && entry.file_name().as_bytes().ends_with(b".lock") {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

/// Where [`Shadow::open`] tightens directory modes from: Eitri's own `eitri` directory when
/// `review_dir` is `<state home>/eitri/review/<key>`, so a state directory an older build left
/// open is closed as every other state writer closes it; otherwise (a store placed anywhere else)
/// only from `review_dir`'s parent down, never touching what is above.
pub(crate) fn private_root(review_dir: &Path) -> &Path {
    let parent = review_dir.parent().unwrap_or(review_dir);
    match parent.parent() {
        Some(eitri)
            if parent.file_name() == Some(OsStr::new("review")) && eitri.file_name() == Some(OsStr::new("eitri")) =>
        {
            eitri
        }
        _ => parent,
    }
}

/// The common git directory from `rev-parse --is-inside-work-tree --show-prefix --git-common-dir`
/// output, when it says the directory asked about is the top of a work tree. The first two values
/// then have a fixed form (`true`, and an empty prefix), so whatever follows them, less git's one
/// trailing newline, is the common directory whole -- even when a path contains a newline.
fn common_dir_at_top(output: &[u8]) -> Option<&OsStr> {
    let common = git::trim_newline(output.strip_prefix(b"true\n\n")?);
    (!common.is_empty()).then(|| OsStr::from_bytes(common))
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `path` as a 0600 file through a uniquely named sibling and a rename, so a
/// concurrent reader sees either the old content or the new, never a half-written file.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let temporary = PathBuf::from(temporary);
    if let Err(e) = agent::private_fs::write_private(&temporary, bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(e);
    }
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_ignored_unicode_aliases_refuse_by_filesystem_identity() {
        for (recorded, target, alias) in [
            ("σ.txt\0", "Σ.txt", "σ.txt"),
            ("é.txt\0", "É.txt", "é.txt"),
            ("café.txt\0", "cafe\u{301}.txt", "café.txt"),
            ("σ/\0", "Σ/new.txt", "σ"),
            ("parent/é/\0", "parent/É/nested/new.txt", "parent/é"),
        ] {
            let ignored = RecordedIgnored(recorded.as_bytes().to_vec());
            let target = Path::new(target);
            let alias = Path::new(alias);
            let equivalent = if recorded.ends_with("/\0") {
                target.ancestors().find(|path| path.parent() == alias.parent()).unwrap()
            } else {
                target
            };
            assert!(!ignored.contains(target));
            assert!(
                ignored
                    .contains_with_stat(target, |path| {
                        Ok((path == alias || path == equivalent).then_some((1, 42)))
                    })
                    .unwrap(),
                "{recorded:?}: {target:?}"
            );
            assert!(!ignored
                .contains_with_stat(target, |path| {
                    Ok(if path == alias { Some((1, 41)) } else { Some((1, 42)) })
                })
                .unwrap());
            assert!(!ignored.contains_directory(equivalent));
        }
    }

    #[test]
    fn recorded_ignored_identity_checks_propagate_errors_and_do_not_match_missing_names() {
        let ignored = RecordedIgnored("é.txt\0".as_bytes().to_vec());
        let target = Path::new("É.txt");
        assert!(!ignored.contains_with_stat(target, |_| Ok(None)).unwrap());
        assert!(ignored
            .contains_with_stat(target, |path| {
                if path == target {
                    Ok(Some((1, 42)))
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EACCES))
                }
            })
            .is_err());
    }

    #[test]
    fn recorded_ignored_paths_are_bounded_and_retained_with_the_snapshot() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-review-ignored", "retention");
        let project = dir.join("project");
        std::fs::create_dir_all(project.join("hidden/nested")).unwrap();
        std::fs::write(project.join(".gitignore"), "hidden/\n*.local\n").unwrap();
        std::fs::write(project.join("hidden/nested/x"), "private").unwrap();
        std::fs::write(project.join("config.local"), "private").unwrap();
        let shadow = Shadow::open_with_excludes(&dir.join("review"), &project, None).unwrap();
        let deadline = Instant::now() + READ_TIMEOUT;
        let at_cap = vec![0; MAX_IGNORED_BYTES];
        assert!(shadow.store_ignored(&at_cap, deadline).unwrap().is_some());
        assert!(shadow
            .store_ignored(&vec![0; MAX_IGNORED_BYTES + 1], deadline)
            .unwrap()
            .is_none());
        let label = SnapshotLabel {
            turn: 1,
            kind: SnapshotKind::End,
            turn_id: "t1".into(),
            tab: 1,
            time_ms: 1,
        };
        let SnapshotOutcome::Taken(snapshot) = shadow.snapshot("s1", &label, &Limits::default()) else {
            panic!("snapshot unavailable");
        };
        shadow.trim(1, Duration::from_secs(3600), 2, "now").unwrap();
        for (path, ignored) in [
            ("config.local", true),
            ("CONFIG.LOCAL", true),
            ("HIDDEN/new", true),
            ("hidden/nested/x", true),
            ("hidden/new", true),
            ("config.local/new", false),
            ("hidden-other/x", false),
            ("new", false),
        ] {
            assert_eq!(
                shadow
                    .recorded_ignored(&snapshot.commit)
                    .unwrap()
                    .map(|list| list.contains(Path::new(path))),
                Some(ignored),
                "{path}"
            );
        }
        assert_eq!(shadow.snapshot_refs().unwrap().len(), 1);
        assert_eq!(shadow.snapshot_ref_ids().unwrap().len(), 1);
        let ignored_ref = format!("{}-ignored", snapshot.reference);
        let refs = shadow.snapshot_refs_locked(Instant::now() + READ_TIMEOUT).unwrap();
        assert!(refs.iter().any(|(name, label)| name == &ignored_ref && label.is_none()));
        shadow.trim(0, Duration::from_secs(3600), 2, "now").unwrap();
        assert!(shadow
            .snapshot_refs_locked(Instant::now() + READ_TIMEOUT)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ignored_paths_survive_rule_changes_during_snapshot() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-review-ignored", "rule-change");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".gitignore"), "before\n").unwrap();
        std::fs::write(project.join("before"), "old").unwrap();
        std::fs::write(project.join("after"), "old").unwrap();
        let shadow = Shadow::open_with_excludes(&dir.join("review"), &project, None).unwrap();
        let ignore = project.join(".gitignore");
        AFTER_LISTING.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || std::fs::write(ignore, "after\n").unwrap()));
        });
        let label = SnapshotLabel {
            turn: 1,
            kind: SnapshotKind::Base,
            turn_id: "t1".into(),
            tab: 1,
            time_ms: 1,
        };
        let SnapshotOutcome::Taken(snapshot) = shadow.snapshot("s1", &label, &Limits::default()) else {
            panic!("snapshot unavailable");
        };
        assert!(AFTER_LISTING.with(|hook| hook.borrow().is_none()));
        for path in ["before", "after"] {
            assert_eq!(
                shadow
                    .recorded_ignored(&snapshot.commit)
                    .unwrap()
                    .map(|list| list.contains(Path::new(path))),
                Some(true),
                "{path}"
            );
        }
    }

    #[test]
    fn only_tree_entries_prove_ancestor_directories() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-review-tree-mode", "entries");
        let project = dir.join("project");
        std::fs::create_dir_all(project.join("directory")).unwrap();
        std::fs::write(project.join("directory/file"), "text").unwrap();
        std::os::unix::fs::symlink("directory", project.join("link")).unwrap();
        let shadow = Shadow::open_with_excludes(&dir.join("review"), &project, None).unwrap();
        let label = SnapshotLabel {
            turn: 1,
            kind: SnapshotKind::Base,
            turn_id: "t1".into(),
            tab: 1,
            time_ms: 1,
        };
        let SnapshotOutcome::Taken(snapshot) = shadow.snapshot("s1", &label, &Limits::default()) else {
            panic!("snapshot unavailable");
        };
        assert!(shadow.tree_contains(&snapshot.commit, Path::new("directory")).unwrap());
        assert!(!shadow.tree_contains(&snapshot.commit, Path::new("link")).unwrap());
        assert!(!shadow
            .tree_contains(&snapshot.commit, Path::new("directory/file"))
            .unwrap());
        let mut tree = shadow.git(None);
        tree.arg("mktree");
        let input = format!("160000 commit {}\tsubmodule\n", snapshot.commit);
        let tree = shadow
            .run_ok("mktree", tree, Some(input.as_bytes()), Instant::now() + READ_TIMEOUT)
            .unwrap();
        let tree = String::from_utf8(tree.stdout).unwrap();
        assert!(!shadow.tree_contains(tree.trim(), Path::new("submodule")).unwrap());
    }

    #[test]
    fn trim_reports_deleted_turns_even_when_collection_later_fails() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-review-trim", "notification");
        let project = dir.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("file"), "text").unwrap();
        let shadow = Shadow::open_with_excludes(&dir.join("review"), &project, None).unwrap();
        for turn in 1..=2 {
            for kind in [SnapshotKind::Base, SnapshotKind::End] {
                let label = SnapshotLabel {
                    turn,
                    kind,
                    turn_id: format!("t{turn}"),
                    tab: 1,
                    time_ms: u64::from(turn),
                };
                assert!(matches!(
                    shadow.snapshot("s1", &label, &Limits::default()),
                    SnapshotOutcome::Taken(_)
                ));
            }
        }
        let removed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let notified = removed.clone();
        let config = shadow.git_dir().join("config");
        super::super::test_hooks::set(&super::super::test_hooks::BEFORE_GC, move || {
            assert_eq!(*notified.lock().unwrap(), Some(vec![("s1".to_owned(), 1)]));
            std::fs::write(config, "[invalid config").unwrap();
        });
        let result = shadow.trim_notifying(1, Duration::from_secs(3600), 3, "now", |turns| {
            *removed.lock().unwrap() = Some(turns);
        });
        assert!(result.is_err(), "the ref deletion preceded the failed collection");
        assert_eq!(*removed.lock().unwrap(), Some(vec![("s1".to_owned(), 1)]));
    }

    thread_local! {
        /// Run once, on this thread, between a snapshot's listing of the work tree and its add: the
        /// moment another program's write lands in when it races a snapshot.
        pub(super) static AFTER_LISTING: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
        /// Run once, on this thread, right after a snapshot's add; an error from it stands for a run
        /// whose time ran out before it measured what the add stored.
        pub(super) static AFTER_ADD: std::cell::RefCell<Option<Box<dyn FnOnce() -> Result<(), ShadowError>>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A file made over the size limit between the listing and the add is left out all the same,
    /// in that snapshot and in the next, which finds it unchanged.
    #[test]
    fn a_file_made_large_while_a_snapshot_runs_is_left_out() {
        use std::os::unix::fs::DirBuilderExt;
        let scratch = std::env::temp_dir().join(format!("eitri-shadow-racing-write-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&scratch).unwrap();
        let scratch = scratch.canonicalize().unwrap();
        let project = scratch.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("small.txt"), b"small\n").unwrap();
        let review_dir = review_dir_for(Some(scratch.join("state").as_os_str()), None, &project).unwrap();
        let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();
        let limits = Limits {
            max_file_bytes: 64,
            ..Limits::default()
        };
        let label = |kind| SnapshotLabel {
            turn: 1,
            kind,
            turn_id: "t1".into(),
            tab: 1,
            time_ms: 1,
        };
        let in_tree = |commit: &str| {
            let mut cmd = shadow.git(None);
            cmd.args(["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit]);
            let out = shadow
                .run_ok("ls-tree", cmd, None, Instant::now() + READ_TIMEOUT)
                .unwrap();
            nul_separated(&out.stdout)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>()
        };
        let taken = |outcome| match outcome {
            SnapshotOutcome::Taken(snapshot) => snapshot,
            SnapshotOutcome::Unavailable(why) => panic!("snapshot unavailable: {why}"),
        };

        let big = project.join("big.txt");
        AFTER_LISTING.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || std::fs::write(&big, [b'x'; 100]).unwrap()));
        });
        let first = taken(shadow.snapshot("s1", &label(SnapshotKind::Warm), &limits));
        assert!(AFTER_LISTING.with(|hook| hook.borrow().is_none()), "the write ran");
        assert_eq!(in_tree(&first.commit), ["small.txt"]);
        assert_eq!(first.skipped_large, [PathBuf::from("big.txt")]);
        let print = first.left_out.get(Path::new("big.txt")).copied().flatten();
        assert_eq!(print.map(|p| p.len), Some(100), "printed as it is on disk");

        let second = taken(shadow.snapshot("s1", &label(SnapshotKind::Base), &limits));
        assert_eq!(in_tree(&second.commit), ["small.txt"]);
        assert_eq!(second.left_out, first.left_out, "unchanged, and so the same print");
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// A snapshot that fails after its add, with a file made over the limit in between, leaves no
    /// unmeasured blob behind for the next one to find unchanged and keep.
    #[test]
    fn a_snapshot_that_fails_after_its_add_leaves_nothing_unmeasured() {
        use std::os::unix::fs::DirBuilderExt;
        let scratch = std::env::temp_dir().join(format!("eitri-shadow-failed-add-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&scratch).unwrap();
        let scratch = scratch.canonicalize().unwrap();
        let project = scratch.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("small.txt"), b"small\n").unwrap();
        let review_dir = review_dir_for(Some(scratch.join("state").as_os_str()), None, &project).unwrap();
        let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();
        let limits = Limits {
            max_file_bytes: 64,
            ..Limits::default()
        };
        let label = |kind| SnapshotLabel {
            turn: 1,
            kind,
            turn_id: "t1".into(),
            tab: 1,
            time_ms: 1,
        };

        let big = project.join("big.txt");
        AFTER_LISTING.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || std::fs::write(&big, [b'x'; 100]).unwrap()));
        });
        AFTER_ADD.with(|hook| *hook.borrow_mut() = Some(Box::new(|| Err(ShadowError::TimedOut))));
        assert!(matches!(
            shadow.snapshot("s1", &label(SnapshotKind::Warm), &limits),
            SnapshotOutcome::Unavailable(_)
        ));
        assert!(AFTER_ADD.with(|hook| hook.borrow().is_none()), "the failure ran");

        let SnapshotOutcome::Taken(next) = shadow.snapshot("s1", &label(SnapshotKind::Base), &limits) else {
            panic!("the next snapshot is taken");
        };
        let mut cmd = shadow.git(None);
        cmd.args(["ls-tree", "-r", "-z", "--name-only", "--full-tree", &next.commit]);
        let out = shadow
            .run_ok("ls-tree", cmd, None, Instant::now() + READ_TIMEOUT)
            .unwrap();
        let names: Vec<String> = nul_separated(&out.stdout)
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        assert_eq!(names, ["small.txt"]);
        assert_eq!(next.skipped_large, [PathBuf::from("big.txt")]);
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// The access mode `file` was opened with.
    fn access_mode(file: &File) -> libc::c_int {
        // SAFETY: the descriptor is `file`'s own and open for the call; `F_GETFL` takes no pointer.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "{}", std::io::Error::last_os_error());
        flags & libc::O_ACCMODE
    }

    /// An exclusive `flock` on NFS needs a descriptor open for writing; this pins it on every file
    /// system, since a local one takes the lock either way and would not notice the difference.
    #[test]
    fn a_probe_lock_is_taken_on_a_descriptor_open_for_writing() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-shadow", "probe-mode");
        let path = dir.join("windows-1");
        std::fs::write(&path, b"").unwrap();
        let probe = try_flock_existing(&path).unwrap().expect("nobody holds it");
        assert_eq!(access_mode(&probe.file), libc::O_RDWR);
    }

    #[test]
    fn a_label_round_trips_through_its_message() {
        let label = SnapshotLabel {
            turn: 7,
            kind: SnapshotKind::End,
            turn_id: "t\n1".into(),
            tab: 3,
            time_ms: 1_790_000_000_000,
        };
        let parsed = SnapshotLabel::parse(&label.to_message()).unwrap();
        assert_eq!(
            parsed,
            SnapshotLabel {
                turn_id: "t 1".into(),
                ..label
            }
        );
    }

    #[test]
    fn a_snapshots_message_keeps_its_time_and_its_left_out_files() {
        let label = SnapshotLabel {
            turn: 2,
            kind: SnapshotKind::Base,
            turn_id: "t2".into(),
            tab: 1,
            time_ms: 5,
        };
        let odd = PathBuf::from(OsString::from_vec(b"big\nname\xff.bin".to_vec()));
        let print = FilePrint {
            len: 9 * 1024 * 1024,
            mtime: (1_790_000_000, 123_456_789),
            ctime: (1_790_000_001, 5),
            ino: 4242,
        };
        let skipped = vec![
            (PathBuf::from("assets/huge.bin"), Some(print)),
            (odd.clone(), Some(FilePrint { ino: 7, ..print })),
            (PathBuf::from("no-print.bin"), None),
        ];
        let message = snapshot_message(&label, 1_790_000_000_123, &skipped);
        assert_eq!(
            SnapshotLabel::parse(&message),
            Some(label),
            "the label still reads back"
        );
        assert_eq!(parse_taken(&message), Some(1_790_000_000_123));
        assert_eq!(
            parse_skipped_large(&message),
            vec![PathBuf::from("assets/huge.bin"), odd, PathBuf::from("no-print.bin")]
        );
        assert_eq!(parse_left_out(&message), skipped.iter().cloned().collect::<LeftOut>());
        assert_eq!(TurnMarks::parse(&message), None);
        // A message from before these lines existed reads as nothing taken, nothing left out.
        let old = SnapshotLabel::parse(&message).unwrap().to_message();
        assert_eq!(parse_taken(&old), None);
        assert!(parse_skipped_large(&old).is_empty());
        assert!(parse_left_out(&old).is_empty());
    }

    #[test]
    fn a_message_from_before_prints_were_kept_lists_its_files_without_one() {
        let message = format!(
            "kind: base\nturn: 1\nturn-id: t1\ntab: 1\ntime: 5\ntaken: 9\nskipped-large: {}\n",
            hex_encode(b"huge.txt")
        );
        assert_eq!(
            parse_left_out(&message),
            LeftOut::from([(PathBuf::from("huge.txt"), None)])
        );
        // A print line that does not read back is dropped, not guessed at.
        let torn = format!("{message}skipped-print: {} 1 2 3\n", hex_encode(b"huge.txt"));
        assert_eq!(
            parse_left_out(&torn),
            LeftOut::from([(PathBuf::from("huge.txt"), None)])
        );
    }

    #[test]
    fn marks_round_trip_and_are_no_snapshot_label() {
        let marks = TurnMarks {
            late: true,
            overlapped_next: false,
            overlapped_tab: true,
            left_running: true,
        };
        let message = marks.to_message(4, "t\n4");
        assert_eq!(TurnMarks::parse(&message), Some(marks));
        assert!(TurnMarks {
            left_running: true,
            ..TurnMarks::default()
        }
        .any());
        // A message written before the last mark existed reads it as not set.
        let older = "kind: marks\nturn: 4\nturn-id: t\nlate: true\noverlapped-next: false\noverlapped-tab: true\n";
        assert_eq!(
            TurnMarks::parse(older),
            Some(TurnMarks {
                left_running: false,
                ..marks
            })
        );
        assert_eq!(SnapshotLabel::parse(&message), None);
        let reference = |name: &str| SnapshotRef {
            name: name.into(),
            commit: String::new(),
            label: None,
            taken_ms: None,
            skipped_large: Vec::new(),
            left_out: LeftOut::new(),
            marks: None,
        };
        let marks_ref = reference("refs/eitri/s1/4/marks");
        assert_eq!(marks_ref.parts(), None);
        assert_eq!(marks_ref.session_turn(), Some(("s1", 4)));
        assert_eq!(reference("refs/eitri/s1/4/end").session_turn(), Some(("s1", 4)));
        assert_eq!(reference("refs/eitri/s1/4/other").session_turn(), None);
    }

    #[test]
    fn only_the_top_of_a_work_tree_has_a_common_dir() {
        assert_eq!(common_dir_at_top(b"true\n\n.git\n"), Some(OsStr::new(".git")));
        assert_eq!(
            common_dir_at_top(b"true\n\n/a/b\nc/.git\n"),
            Some(OsStr::new("/a/b\nc/.git"))
        );
        for not_top in [&b"true\nsub/\n../.git\n"[..], b"false\n\n.\n", b"true\n\n\n", b""] {
            assert_eq!(common_dir_at_top(not_top), None, "{not_top:?}");
        }
    }

    #[test]
    fn the_private_root_stops_at_eitris_own_directory() {
        assert_eq!(
            private_root(Path::new("/s/eitri/review/0123456789abcdef")),
            Path::new("/s/eitri")
        );
        assert_eq!(private_root(Path::new("/s/elsewhere/key")), Path::new("/s/elsewhere"));
    }

    #[test]
    fn session_ids_that_could_escape_a_ref_or_a_file_name_are_refused() {
        for bad in ["", "..", "a/b", "a.lock", "a b", "a\nb", &"x".repeat(129)] {
            assert!(validate_session(bad).is_err(), "{bad:?}");
        }
        assert!(validate_session("0b9c2d6e-1f2a-4c3b-9d8e-7f6a5b4c3d2e").is_ok());
    }
}
