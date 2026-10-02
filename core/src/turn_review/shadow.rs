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
//!   exclusively only to delete refs and collect garbage, so a collection never removes an object a
//!   concurrent snapshot (in this window or another one on the same project) has written but not
//!   yet referenced.
//!
//! Each snapshot is a root commit under `refs/eitri/<session>/<turn>/<base|end|warm>`, its
//! message the [`SnapshotLabel`] as `key: value` lines, then when it finished and the files it left
//! out for their size. What a turn's review must say about its precision is a commit of the empty
//! tree under `refs/eitri/<session>/<turn>/marks` ([`TurnMarks`]). There is no other state: the refs are the
//! index of what exists, and deleting a ref really frees its snapshot, since no parent chain keeps
//! it alive.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use super::git::{self, GitError};

/// What the shadow's `info/attributes` says: no text conversion, no filter, no `$Id$` expansion
/// and no encoding conversion, for every path.
pub const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding\n";

/// How long a blob read, a lock wait outside a snapshot, or opening the store may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The limits one snapshot works within. A snapshot that cannot stay inside them says so instead
/// of presenting a partial tree as complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// A file larger than this is left out of the snapshot and listed in
    /// [`Snapshot::skipped_large`].
    pub max_file_bytes: u64,
    /// More files than this to add leaves the turn without a snapshot.
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
fn snapshot_message(label: &SnapshotLabel, taken_ms: u64, skipped_large: &[PathBuf]) -> String {
    let mut message = label.to_message();
    message.push_str(&format!("taken: {taken_ms}\n"));
    for path in skipped_large {
        // Hex, because a file name may hold a line break or bytes that are not UTF-8.
        message.push_str("skipped-large: ");
        message.push_str(&hex_encode(path.as_os_str().as_bytes()));
        message.push('\n');
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

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
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
}

impl TurnMarks {
    pub fn any(&self) -> bool {
        self.late || self.overlapped_next || self.overlapped_tab
    }

    fn to_message(self, turn: u32, turn_id: &str) -> String {
        let turn_id: String = turn_id
            .chars()
            .map(|c| if c == '\n' || c == '\r' || c == '\0' { ' ' } else { c })
            .collect();
        format!(
            "kind: marks\nturn: {turn}\nturn-id: {turn_id}\nlate: {}\noverlapped-next: {}\noverlapped-tab: {}\n",
            self.late, self.overlapped_next, self.overlapped_tab
        )
    }

    /// Reads a message written by `to_message` back; `None` for any other message.
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
    /// More files to add than [`Limits::max_files`].
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
    let base = crate::layout::persist::state_subdir(state_home, home, "review")?;
    Some(base.join(&agent::conversation_id_for_cwd(project_root)[..16]))
}

/// A held `flock` on the store's `lock` file, released on drop.
#[derive(Debug)]
pub struct ShadowLock {
    _file: File,
}

/// One project's shadow repository. Cheap to clone; every method takes the lock it needs.
#[derive(Debug, Clone)]
pub struct Shadow {
    review_dir: PathBuf,
    git_dir: PathBuf,
    work_tree: PathBuf,
    excludes_file: Option<PathBuf>,
}

impl Shadow {
    /// Opens (creating on first use) the store in `review_dir` for the project at `work_tree`,
    /// honouring the user's global excludes file.
    pub fn open(review_dir: &Path, work_tree: &Path) -> Result<Shadow, ShadowError> {
        Self::open_with_excludes(review_dir, work_tree, git::user_excludes_file())
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
    /// only while the store lock is held, so a collection, which holds that lock exclusively,
    /// never meets a held guard and may remove the file.
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
    fn clear_stale_locks(&self) -> std::io::Result<()> {
        remove_lock_files(&self.review_dir, false)?;
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
            Some(path) => match std::fs::read(&path) {
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
        match self.snapshot_until(session, label, limits, deadline) {
            Ok(snapshot) => SnapshotOutcome::Taken(snapshot),
            Err(e) => SnapshotOutcome::Unavailable(e.to_string()),
        }
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
        let (tree, skipped_large) = match self.write_tree(&index, limits, deadline) {
            Ok(done) => done,
            // An index can point at objects a collection has since removed (a snapshot that never
            // got its ref, an hour or more ago); starting from an empty index rebuilds it.
            Err(ShadowError::Git(GitError::Failed { what, .. })) if what == "write-tree" => {
                remove_if_present(&index)?;
                self.write_tree(&index, limits, deadline)?
            }
            Err(e) => return Err(e),
        };
        let mut commit = self.git(None);
        commit.args(["commit-tree", &tree, "-F", "-"]);
        let message = snapshot_message(label, wall_clock_ms(), &skipped_large);
        let commit = self.run_ok("commit-tree", commit, Some(message.as_bytes()), deadline)?;
        let commit = String::from_utf8_lossy(git::trim_newline(&commit.stdout)).into_owned();
        let reference = format!("refs/eitri/{session}/{}/{}", label.turn, label.kind.as_str());
        // Only this session's snapshots, under its guard, and a collection, under the exclusive
        // store lock, write this ref, so a lock on it now is stale too.
        remove_if_present(&lock_file_of(&self.git_dir.join(&reference)))?;
        let mut update = self.git(None);
        update.args(["update-ref", &reference, &commit]);
        self.run_ok("update-ref", update, None, deadline)?;
        Ok(Snapshot {
            commit,
            tree,
            reference,
            skipped_large,
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
    ///
    /// Nothing references the returned tree; it stays readable until a collection prunes it, so a
    /// caller that holds [`lock_shared`](Self::lock_shared) from before this call until its last
    /// read of the tree is safe from that.
    pub fn scratch_tree(&self, limits: &Limits) -> Result<String, ShadowError> {
        let deadline = Instant::now() + limits.timeout;
        let session = format!("scratch-{}", uuid::Uuid::new_v4().simple());
        let index = self.index_path(&session)?;
        let _lock = self.lock_shared_until(deadline)?;
        self.sync_excludes_until(deadline)?;
        let written = self.write_tree(&index, limits, deadline);
        let _ = remove_if_present(&index);
        // A run killed for its time leaves the lock; nothing else will ever use this index.
        let _ = remove_if_present(&lock_file_of(&index));
        Ok(written?.0)
    }

    /// Brings `index` up to date with the work tree and writes its tree, returning the tree and the
    /// files left out for size. The caller holds the lock.
    fn write_tree(
        &self,
        index: &Path,
        limits: &Limits,
        deadline: Instant,
    ) -> Result<(String, Vec<PathBuf>), ShadowError> {
        let mut list = self.git(Some(index));
        list.args(["ls-files", "-z", "--others", "--modified", "--exclude-standard"]);
        let listed = self.run_ok("ls-files", list, None, deadline)?;
        let entries: BTreeSet<&[u8]> = nul_separated(&listed.stdout).collect();
        if entries.len() > limits.max_files {
            return Err(ShadowError::TooManyFiles {
                count: entries.len(),
                max: limits.max_files,
            });
        }

        let mut large: Vec<&[u8]> = Vec::new();
        let mut left_out: Vec<&[u8]> = Vec::new();
        for entry in &entries {
            if let Some(dir) = entry.strip_suffix(b"/") {
                // An untracked nested repository: one gitlink entry, unless it has no commit yet,
                // which git refuses to add at all.
                if !self.nested_has_commit(dir, deadline)? {
                    left_out.push(dir);
                }
                continue;
            }
            let path = self.work_tree.join(OsStr::from_bytes(entry));
            if let Ok(meta) = path.symlink_metadata() {
                if meta.is_file() && meta.len() > limits.max_file_bytes {
                    large.push(entry);
                }
            }
        }

        if !large.is_empty() {
            let mut remove = self.git(Some(index));
            remove.args(["update-index", "-z", "--force-remove", "--stdin"]);
            self.run_ok("update-index", remove, Some(&nul_joined(&large)), deadline)?;
        }

        let mut ignored = self.git(Some(index));
        ignored.args(["ls-files", "-z", "--cached", "--ignored", "--exclude-standard"]);
        let ignored = self.run_ok("ls-files", ignored, None, deadline)?;
        if !ignored.stdout.is_empty() {
            let mut remove = self.git(Some(index));
            remove.args(["update-index", "-z", "--force-remove", "--stdin"]);
            self.run_ok("update-index", remove, Some(&ignored.stdout), deadline)?;
        }

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

        let mut write = self.git(Some(index));
        write.arg("write-tree");
        let tree = self.run_ok("write-tree", write, None, deadline)?;
        let tree = String::from_utf8_lossy(git::trim_newline(&tree.stdout)).into_owned();
        let skipped = large
            .iter()
            .map(|p| PathBuf::from(OsString::from_vec(p.to_vec())))
            .collect();
        Ok((tree, skipped))
    }

    /// Whether the nested repository at `dir` (relative to the work tree) has a commit checked out.
    /// Read-only, and isolated like every other shadow git run.
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

    /// Every snapshot ref, with its label (`None` when its message is not a label).
    pub fn snapshot_refs(&self) -> Result<Vec<(String, Option<SnapshotLabel>)>, ShadowError> {
        let _lock = self.lock_shared_until(Instant::now() + READ_TIMEOUT)?;
        self.snapshot_refs_locked(Instant::now() + READ_TIMEOUT)
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
            refs.push(SnapshotRef {
                name,
                commit: String::from_utf8_lossy(commit).into_owned(),
                label: SnapshotLabel::parse(&message),
                taken_ms: parse_taken(&message),
                skipped_large: parse_skipped_large(&message),
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
    /// cannot point at objects the collection frees. Holds the lock exclusively throughout.
    pub fn trim(&self, keep: usize, max_age: Duration, now_ms: u64, prune_expiry: &str) -> Result<(), ShadowError> {
        let _lock = self.lock_exclusive()?;
        // Collection is not bounded by the snapshot timeout: it runs in the background, and
        // stopping it halfway only wastes the work.
        let deadline = Instant::now() + Duration::from_secs(600);
        // A ref lock or `packed-refs.lock` left by a killed snapshot or collection would make the
        // ref deletion below fail, and with it every collection from then on.
        self.clear_stale_locks()?;
        let refs = self.snapshot_refs_locked(deadline)?;

        // Turn -> (newest label time, its refs).
        let mut turns: BTreeMap<(String, String), (u64, Vec<String>)> = BTreeMap::new();
        for (name, label) in &refs {
            let Some(rest) = name.strip_prefix("refs/eitri/") else {
                continue;
            };
            let mut parts = rest.split('/');
            let (Some(session), Some(turn)) = (parts.next(), parts.next()) else {
                continue;
            };
            let entry = turns.entry((session.to_owned(), turn.to_owned())).or_default();
            entry.0 = entry.0.max(label.as_ref().map_or(0, |l| l.time_ms));
            entry.1.push(name.clone());
        }
        let mut ordered: Vec<_> = turns.into_iter().collect();
        ordered.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| b.0.cmp(&a.0)));
        let oldest_kept = now_ms.saturating_sub(u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX));

        let mut delete = Vec::new();
        let mut kept_sessions = BTreeSet::new();
        for (position, ((session, _), (time, names))) in ordered.into_iter().enumerate() {
            if position >= keep || time < oldest_kept {
                delete.extend(names);
            } else {
                kept_sessions.insert(session);
            }
        }
        if !delete.is_empty() {
            let input: String = delete.iter().map(|name| format!("delete {name}\n")).collect();
            let mut update = self.git(None);
            update.args(["update-ref", "--stdin"]);
            self.run_ok("update-ref", update, Some(input.as_bytes()), deadline)?;
        }

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

        let mut gc = self.git(None);
        gc.arg("gc").arg(format!("--prune={prune_expiry}")).arg("--quiet");
        self.run_ok("gc", gc, None, deadline)?;
        Ok(())
    }
}

/// Takes an `flock` (`operation`) on `path`, creating it 0600. With a `deadline` it polls and gives
/// up with [`ShadowError::TimedOut`] there; without one it waits as long as it takes.
fn flock_file(path: &Path, operation: libc::c_int, deadline: Option<Instant>) -> Result<ShadowLock, ShadowError> {
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
    let flags = if deadline.is_some() {
        operation | libc::LOCK_NB
    } else {
        operation
    };
    loop {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(ShadowLock { _file: file });
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

/// A session id is a file-name and ref component: ASCII letters, digits, `-` and `_`, at most 128.
fn validate_session(session: &str) -> Result<(), ShadowError> {
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
fn private_root(review_dir: &Path) -> &Path {
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
        let skipped = vec![PathBuf::from("assets/huge.bin"), odd];
        let message = snapshot_message(&label, 1_790_000_000_123, &skipped);
        assert_eq!(
            SnapshotLabel::parse(&message),
            Some(label),
            "the label still reads back"
        );
        assert_eq!(parse_taken(&message), Some(1_790_000_000_123));
        assert_eq!(parse_skipped_large(&message), skipped);
        assert_eq!(TurnMarks::parse(&message), None);
        // A message from before these lines existed reads as nothing taken, nothing left out.
        let old = SnapshotLabel::parse(&message).unwrap().to_message();
        assert_eq!(parse_taken(&old), None);
        assert!(parse_skipped_large(&old).is_empty());
    }

    #[test]
    fn marks_round_trip_and_are_no_snapshot_label() {
        let marks = TurnMarks {
            late: true,
            overlapped_next: false,
            overlapped_tab: true,
        };
        let message = marks.to_message(4, "t\n4");
        assert_eq!(TurnMarks::parse(&message), Some(marks));
        assert_eq!(SnapshotLabel::parse(&message), None);
        let reference = |name: &str| SnapshotRef {
            name: name.into(),
            commit: String::new(),
            label: None,
            taken_ms: None,
            skipped_large: Vec::new(),
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
