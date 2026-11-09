//! Writing a proposal to the project: a change computed for an agent, held as two blobs per file in
//! turn review's store, and written only while the disk still holds the bytes it was computed from.
//!
//! A proposal is applied inside a turn, like any edit tool's write, so a revert's preconditions do
//! not all fit: [`PresenceGuard::check`](super::presence::PresenceGuard::check) refuses while a turn
//! runs, and a person's card, not a revert key, is what asks for this write. What guards the bytes
//! themselves stays:
//!
//! - **the write slot**, held for the whole apply, so a revert, an undo, a recovery, a review message
//!   and another apply exclude it in every window;
//! - **the reach from the root**: every file through [`ProjectDir`], no link followed, nothing by a
//!   path string;
//! - **the expected bytes**, compared for every file before the first write, once more right before
//!   each write, and a last time inside [`replace`].
//!
//! Files are checked whole and written one at a time, with no way back: a failure after the first
//! write stops, and the report says what landed. Turn review sees those as it sees any disk change.
//! An in-place write journals like a revert's, so a crash leaves an entry in the same recovery list.
//!
//! A blob here has no anchor: a proposal lives for minutes, and a collection that took one answers
//! [`ProposalRefused::Retained`].

use std::fmt;
use std::path::{Path, PathBuf};

use super::diff::{blob_hunks, Hunk, LineKind};
use super::journal::{Journal, JournalNote};
use super::lifecycle::ShadowSpec;
use super::shadow::{validate_session, Shadow, ShadowError};
use super::write::{
    file_len, file_modified_ms, read_current_within, replace, write_mode, Content, Current, FsHooks, ProjectDir,
    Target, WriteError, WriteMode,
};
use super::write_slot::{SlotBusy, WriteSlot};
use crate::scratch::PROPOSAL_MAX_BYTES;

/// One project's way to read, store and write proposed files. Each call opens the store or the disk,
/// so all of them run on a worker, never the GTK thread.
#[derive(Debug, Clone)]
pub struct ProposalWriter {
    spec: ShadowSpec,
}

/// A project file as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNow {
    pub bytes: Vec<u8>,
    pub mode: u32,
    /// When the file last changed, in milliseconds since the epoch, asked after the bytes were read: a change that
    /// lands between the two reads shows here.
    pub modified_ms: u64,
}

/// One file of a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedFile {
    /// Relative to the project root.
    pub rel: PathBuf,
    /// The blob of the bytes the change was computed from.
    pub before: String,
    /// The blob of the bytes the change leaves.
    pub after: String,
    /// The file's permission bits when it was read.
    pub mode: u32,
    /// The change as a unified patch: `@@` headers and their lines.
    pub hunks: String,
}

/// Why a proposal was not read, stored or written. The sentence names the file when one file is why;
/// [`Retained`](Self::Retained), [`NoStore`](Self::NoStore), [`Busy`](Self::Busy) and an
/// [`Unavailable`](Self::Unavailable) about the store or the project concern the whole proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalRefused {
    /// There is no state directory, so there is nowhere to keep a proposal's bytes.
    NoStore,
    /// The path leaves the project, or reaches it through a link.
    Outside(String),
    /// Nothing is at the path, or something other than a regular file.
    NotAFile(String),
    /// The file, or what it would become, is over [`PROPOSAL_MAX_BYTES`].
    TooLarge(String),
    /// The two sides differ and git shows no patch between them.
    NoHunks(String),
    /// The store no longer holds a blob of this proposal.
    Retained,
    /// The project's write slot is taken.
    Busy(SlotBusy),
    /// Anything else, the disk differing from the proposal included.
    Unavailable(String),
}

impl fmt::Display for ProposalRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProposalRefused::NoStore => f.write_str("no state directory to keep the proposal in"),
            ProposalRefused::Retained => {
                f.write_str("the stored copy of this proposal is gone; nothing was written, ask for it again")
            }
            ProposalRefused::Busy(busy) => write!(f, "{busy}"),
            ProposalRefused::Outside(why)
            | ProposalRefused::NotAFile(why)
            | ProposalRefused::TooLarge(why)
            | ProposalRefused::NoHunks(why)
            | ProposalRefused::Unavailable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ProposalRefused {}

/// What an apply did. A write that failed stops the rest: `written` stays on disk. The sentence in
/// `failed` speaks of that file alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub written: Vec<PathBuf>,
    /// The file that failed, and why.
    pub failed: Option<(PathBuf, String)>,
    pub not_tried: Vec<PathBuf>,
}

impl ProposalWriter {
    /// The writer of the project at `canonical_root`, its blobs in `review_dir` (`None` when there is
    /// no state directory: a file can be read, nothing can be stored or written).
    pub fn new(review_dir: Option<PathBuf>, canonical_root: &Path) -> ProposalWriter {
        ProposalWriter::on_spec(ShadowSpec {
            review_dir,
            root: canonical_root.to_path_buf(),
            excludes: None,
        })
    }

    pub(super) fn on_spec(spec: ShadowSpec) -> ProposalWriter {
        ProposalWriter { spec }
    }

    /// The regular file at `rel`, never following a link. A language server can name any file, so
    /// its size is asked first and an oversized one is refused before a byte of it is read.
    pub fn read(&self, rel: &Path) -> Result<FileNow, ProposalRefused> {
        let shown = rel.display();
        let unreadable = |e: std::io::Error| ProposalRefused::Unavailable(format!("{shown} could not be read: {e}"));
        let dir = self.project()?;
        let target = reach(&dir, rel)?;
        if let Some(len) = file_len(&target).map_err(unreadable)? {
            if len > PROPOSAL_MAX_BYTES {
                return Err(ProposalRefused::TooLarge(format!(
                    "{shown} is too large to propose a change to ({len} bytes; the most is {PROPOSAL_MAX_BYTES})"
                )));
            }
        }
        #[cfg(test)]
        tests::run_hook(&tests::AFTER_THE_SIZE);
        // The file may grow after its size was asked, so the read has its own bound.
        match read_current_within(&target, PROPOSAL_MAX_BYTES).map_err(unreadable)? {
            Current::Regular { bytes, .. } if bytes.len() as u64 > PROPOSAL_MAX_BYTES => {
                Err(ProposalRefused::TooLarge(format!(
                    "{shown} grew past {PROPOSAL_MAX_BYTES} bytes while it was being read"
                )))
            }
            Current::Regular { bytes, mode } => match file_modified_ms(&target).map_err(unreadable)? {
                Some(modified_ms) => Ok(FileNow {
                    bytes,
                    mode,
                    modified_ms,
                }),
                None => Err(ProposalRefused::NotAFile(format!("{shown}: no such file"))),
            },
            Current::Absent => Err(ProposalRefused::NotAFile(format!("{shown}: no such file"))),
            Current::Symlink { .. } => Err(ProposalRefused::NotAFile(format!("{shown} is a link, not a file"))),
            Current::Other => Err(ProposalRefused::NotAFile(format!("{shown} is not a regular file"))),
        }
    }

    /// Stores both sides of a change to `rel` and works out its patch. A pair git calls binary has no
    /// patch, and a change the card cannot show must not stand, so it is refused here.
    pub fn store(&self, rel: &Path, now: &FileNow, after: &[u8]) -> Result<ProposedFile, ProposalRefused> {
        let shown = rel.display();
        if after.len() as u64 > PROPOSAL_MAX_BYTES {
            return Err(ProposalRefused::TooLarge(format!(
                "the change would make {shown} too large ({} bytes; the most is {PROPOSAL_MAX_BYTES})",
                after.len()
            )));
        }
        let shadow = self.open_store()?;
        let failed = |e: ShadowError| ProposalRefused::Unavailable(format!("{shown} could not be stored: {e}"));
        let before = shadow.store_blob(&now.bytes).map_err(failed)?;
        let after = shadow.store_blob(after).map_err(failed)?;
        let hunks = blob_hunks(&shadow, &before, &after).map_err(|e| match e {
            ShadowError::NoSuchCommit(_) => ProposalRefused::Retained,
            e => failed(e),
        })?;
        if hunks.is_empty() && before != after {
            return Err(ProposalRefused::NoHunks(format!(
                "git shows no patch for the change to {shown} (a binary result), so it cannot be proposed"
            )));
        }
        Ok(ProposedFile {
            rel: rel.to_path_buf(),
            before,
            after,
            mode: now.mode,
            hunks: patch_text(&hunks),
        })
    }

    /// Both sides of `file` as text, from the store: `(before, after)`.
    pub fn texts(&self, file: &ProposedFile) -> Result<(String, String), ProposalRefused> {
        let shadow = self.open_store()?;
        let text = |blob: &str| -> Result<String, ProposalRefused> {
            String::from_utf8(stored(&shadow, blob, &file.rel)?)
                .map_err(|_| ProposalRefused::Unavailable(format!("{} is not UTF-8 text", file.rel.display())))
        };
        Ok((text(&file.before)?, text(&file.after)?))
    }

    /// Writes `files`, each from its `before` to its `after`, for `session`. Every file is looked at
    /// before the first write, so a refusal (`Err`) has written nothing; once writing has started, a
    /// failure stops and the report says what landed.
    pub fn apply(&self, session: &str, files: &[ProposedFile]) -> Result<ApplyReport, ProposalRefused> {
        self.apply_with(session, files, &FsHooks::default())
    }

    fn apply_with(
        &self,
        session: &str,
        files: &[ProposedFile],
        hooks: &FsHooks,
    ) -> Result<ApplyReport, ProposalRefused> {
        let review_dir = self.spec.review_dir.as_deref().ok_or(ProposalRefused::NoStore)?;
        let slot = WriteSlot::open_or_busy(review_dir).map_err(ProposalRefused::Busy)?;
        let _hold = slot.try_exclusive().map_err(ProposalRefused::Busy)?;
        let shadow = self.open_store()?;
        let journal = Journal::open(shadow.review_dir())
            .map_err(|e| ProposalRefused::Unavailable(format!("the review store could not be opened: {e}")))?;
        let dir = self.project()?;

        for file in files {
            check_before_writing(&shadow, &dir, session, file)?;
        }
        #[cfg(test)]
        tests::run_hook(&tests::BETWEEN_THE_LOOKS);

        let mut written = Vec::with_capacity(files.len());
        let mut rest = files.iter();
        while let Some(file) = rest.next() {
            if let Err(why) = write_one(&shadow, &journal, &dir, session, file, hooks) {
                return Ok(ApplyReport {
                    written,
                    failed: Some((file.rel.clone(), why)),
                    not_tried: rest.map(|left| left.rel.clone()).collect(),
                });
            }
            written.push(file.rel.clone());
        }
        Ok(ApplyReport {
            written,
            failed: None,
            not_tried: Vec::new(),
        })
    }

    fn project(&self) -> Result<ProjectDir, ProposalRefused> {
        ProjectDir::open(&self.spec.root).map_err(|e| {
            ProposalRefused::Unavailable(format!(
                "the project at {} could not be opened: {e}",
                self.spec.root.display()
            ))
        })
    }

    fn open_store(&self) -> Result<Shadow, ProposalRefused> {
        if self.spec.review_dir.is_none() {
            return Err(ProposalRefused::NoStore);
        }
        self.spec
            .open()
            .map_err(|why| ProposalRefused::Unavailable(format!("the review store could not be opened: {why}")))
    }
}

/// Reaches `rel` from the project root, creating nothing: a proposal only ever replaces a file.
fn reach(dir: &ProjectDir, rel: &Path) -> Result<Target, ProposalRefused> {
    dir.target(rel, false).map_err(|e| match e {
        WriteError::Outside(why) => ProposalRefused::Outside(why),
        WriteError::Io(why) => ProposalRefused::Unavailable(why),
        other => ProposalRefused::Unavailable(other.to_string()),
    })
}

/// The bytes of `blob`, a blob of the proposal for `rel`.
fn stored(shadow: &Shadow, blob: &str, rel: &Path) -> Result<Vec<u8>, ProposalRefused> {
    match shadow.read_stored(blob) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(ProposalRefused::Retained),
        Err(e) => Err(ProposalRefused::Unavailable(format!("{}: {e}", rel.display()))),
    }
}

/// The look at one file before anything is written: its blobs are there, it is reached from the root,
/// it holds exactly the bytes the proposal was computed from, and a write to it can go ahead.
fn check_before_writing(
    shadow: &Shadow,
    dir: &ProjectDir,
    session: &str,
    file: &ProposedFile,
) -> Result<(), ProposalRefused> {
    let shown = file.rel.display();
    let before = stored(shadow, &file.before, &file.rel)?;
    stored(shadow, &file.after, &file.rel)?;
    let target = reach(dir, &file.rel)?;
    let changed = || ProposalRefused::Unavailable(format!("{shown}: {}", WriteError::Changed));
    let unreadable = |e: std::io::Error| ProposalRefused::Unavailable(format!("{shown} could not be read: {e}"));
    // A file of another size has changed, and is not read to find that out.
    if file_len(&target).map_err(unreadable)? != Some(before.len() as u64) {
        return Err(changed());
    }
    match read_current_within(&target, before.len() as u64).map_err(unreadable)? {
        Current::Regular { bytes, .. } if bytes == before => {}
        _ => return Err(changed()),
    }
    match write_mode(&target).map_err(unreadable)? {
        WriteMode::Atomic => {}
        WriteMode::HardLinked(links) => {
            return Err(ProposalRefused::Unavailable(format!(
                "{shown}: {}",
                WriteError::HardLinked { links }
            )));
        }
        // The journal entry of an in-place write is named by the session, and would refuse this one
        // only after the files before it were written.
        WriteMode::InPlace(_) => validate_session(session)
            .map_err(|e| ProposalRefused::Unavailable(format!("{shown} cannot be journalled: {e}")))?,
    }
    Ok(())
}

/// Writes one checked file, from a fresh reach from the root: a directory moved out of the project
/// since the first look keeps working through a descriptor opened before the move. The mode is the
/// one the file has now, so the write changes no bit of it. The blobs are read again here, so a
/// proposal of many large files is never in memory whole.
fn write_one(
    shadow: &Shadow,
    journal: &Journal,
    dir: &ProjectDir,
    session: &str,
    file: &ProposedFile,
    hooks: &FsHooks,
) -> Result<(), String> {
    let rel = &file.rel;
    let blob = |id: &str| {
        stored(shadow, id, rel).map_err(|e| match e {
            ProposalRefused::Retained => "its stored copy is gone; the file was not written".to_owned(),
            e => e.to_string(),
        })
    };
    let before = blob(&file.before)?;
    let target = dir.target(rel, false).map_err(|e| e.to_string())?;
    let mode = match read_current_within(&target, before.len() as u64) {
        Ok(Current::Regular { bytes, mode }) if bytes == before => mode,
        Ok(_) => return Err(WriteError::Changed.to_string()),
        Err(e) => return Err(format!("it could not be read: {e}")),
    };
    let note = JournalNote {
        session: session.to_owned(),
        path: rel.clone(),
        pre: Some(file.before.clone()),
        pre_mode: mode,
        intended: Some(file.after.clone()),
    };
    let content = Content::Bytes {
        bytes: blob(&file.after)?,
        mode,
    };
    replace(&target, &content, &note, journal, shadow, hooks, None).map_err(|e| e.to_string())
}

/// `hunks` as patch text, which [`parse_unified`](super::diff::parse_unified) reads back.
fn patch_text(hunks: &[Hunk]) -> String {
    let mut text = String::new();
    for hunk in hunks {
        text.push_str(&hunk.header);
        text.push('\n');
        for line in &hunk.lines {
            text.push_str(match line.kind {
                LineKind::Context => " ",
                LineKind::Added => "+",
                LineKind::Removed => "-",
                LineKind::NoNewline => "\\ ",
            });
            text.push_str(&line.text);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use crate::turn_review::diff::parse_unified;
    use crate::turn_review::write::Stage;
    use crate::turn_review::{ReviewOptions, TurnReview};
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    type Hook = RefCell<Option<Box<dyn FnOnce()>>>;

    thread_local! {
        /// Runs after every file was looked at and before the first is written.
        pub(super) static BETWEEN_THE_LOOKS: Hook = const { RefCell::new(None) };
        /// Runs after a read asked the file's size and before it reads a byte.
        pub(super) static AFTER_THE_SIZE: Hook = const { RefCell::new(None) };
        /// What [`remove_at_the_space_check`] removes.
        static REMOVED_AT_THE_SPACE_CHECK: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn run_hook(hook: &'static std::thread::LocalKey<Hook>) {
        if let Some(hook) = hook.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    /// The first thing `replace` asks, after the writer's last read and before `replace` looks.
    fn remove_at_the_space_check(_: &Target) -> std::io::Result<u64> {
        if let Some(path) = REMOVED_AT_THE_SPACE_CHECK.with(|path| path.borrow_mut().take()) {
            std::fs::remove_file(path).unwrap();
        }
        Ok(u64::MAX)
    }

    const SESSION: &str = "session-1";

    /// A project and a store of the test's own.
    struct World {
        _scratch: ScratchDir,
        root: PathBuf,
        review: PathBuf,
        writer: ProposalWriter,
    }

    impl World {
        fn new(label: &str) -> World {
            let scratch = ScratchDir::new("eitri-proposal", label);
            let base = scratch.canonicalize().unwrap();
            let root = base.join("project");
            std::fs::create_dir(&root).unwrap();
            let review = base.join("review");
            let writer = ProposalWriter::on_spec(ShadowSpec {
                review_dir: Some(review.clone()),
                root: root.clone(),
                excludes: Some(None),
            });
            World {
                _scratch: scratch,
                root,
                review,
                writer,
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn write(&self, rel: &str, bytes: &[u8], mode: u32) {
            let path = self.path(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        fn bytes(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.path(rel)).unwrap()
        }

        fn propose(&self, rel: &str, after: &[u8]) -> ProposedFile {
            let now = self.writer.read(Path::new(rel)).unwrap();
            self.writer.store(Path::new(rel), &now, after).unwrap()
        }

        fn journal(&self) -> Journal {
            Journal::open(&self.review).unwrap()
        }

        /// Removes a stored object, as a collection would.
        fn prune(&self, blob: &str) {
            let object = self.review.join("git/objects").join(&blob[..2]).join(&blob[2..]);
            std::fs::remove_file(object).unwrap();
        }
    }

    fn rels(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn sentence(refused: &ProposalRefused) -> String {
        refused.to_string()
    }

    fn assert_unavailable(refused: &ProposalRefused, name: &str, says: &str) {
        let ProposalRefused::Unavailable(text) = refused else {
            panic!("expected a refusal naming {name}, got {refused:?}");
        };
        assert!(text.contains(name), "{text}");
        assert!(text.contains(says), "{text}");
    }

    /// Gives `path` an extended attribute, which makes a write replace it in place. When the file
    /// system refuses user attributes this panics unless `EITRI_TEST_NO_XATTR` is `1`, which makes it
    /// `false` (the caller then has nothing to test and returns).
    fn force_in_place(path: &Path) -> bool {
        let file = CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = c"user.eitri_test";
        // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
        #[cfg(not(target_vendor = "apple"))]
        let set = unsafe { libc::setxattr(file.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0) };
        // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
        #[cfg(target_vendor = "apple")]
        let set = unsafe { libc::setxattr(file.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0, 0) };
        if set == 0 {
            return true;
        }
        let why = std::io::Error::last_os_error();
        if std::env::var_os("EITRI_TEST_NO_XATTR").is_some_and(|v| v == "1") {
            println!("this file system refuses user extended attributes ({why}); skipped");
            return false;
        }
        panic!("this file system refuses user extended attributes ({why}), so the in-place write cannot be tested");
    }

    fn journal_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json"))
            .collect()
    }

    /// Runs `work` where a call that opens a FIFO for reading, and so never returns, fails the test
    /// instead of hanging it.
    fn within_a_moment<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the call is stuck on the file it should not have opened")
    }

    fn make_fifo(path: &Path) {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    }

    #[test]
    fn a_proposal_writes_each_file_and_keeps_its_mode() {
        let world = World::new("writes");
        world.write("run.sh", b"echo one\n", 0o755);
        world.write("src/private.rs", b"const A: u8 = 1;\n", 0o640);
        let files = [
            world.propose("run.sh", b"echo two\n"),
            world.propose("src/private.rs", b"const A: u8 = 2;\n"),
        ];
        let report = world.writer.apply(SESSION, &files).unwrap();
        assert_eq!(
            report,
            ApplyReport {
                written: rels(&["run.sh", "src/private.rs"]),
                failed: None,
                not_tried: Vec::new(),
            }
        );
        assert_eq!(world.bytes("run.sh"), b"echo two\n");
        assert_eq!(world.bytes("src/private.rs"), b"const A: u8 = 2;\n");
        let mode = |rel: &str| std::fs::metadata(world.path(rel)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("run.sh"), 0o755);
        assert_eq!(mode("src/private.rs"), 0o640);
    }

    #[test]
    fn a_mode_changed_since_the_proposal_is_the_mode_kept() {
        let world = World::new("mode-since");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        std::fs::set_permissions(world.path("a.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let report = world.writer.apply(SESSION, &[file]).unwrap();
        assert_eq!(report.written, rels(&["a.txt"]));
        assert_eq!(world.bytes("a.txt"), b"two\n");
        let mode = std::fs::metadata(world.path("a.txt")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o600);
    }

    #[test]
    fn a_file_changed_since_is_refused_and_nothing_is_written() {
        let world = World::new("changed");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        // The same size, so only the bytes tell.
        world.write("a.txt", b"six\n", 0o644);
        let refused = world.writer.apply(SESSION, &[file]).unwrap_err();
        assert_unavailable(&refused, "a.txt", &WriteError::Changed.to_string());
        assert_eq!(world.bytes("a.txt"), b"six\n");
    }

    #[test]
    fn one_changed_file_refuses_every_file() {
        let world = World::new("one-changed");
        world.write("a.txt", b"a one\n", 0o644);
        world.write("b.txt", b"b one\n", 0o644);
        let files = [world.propose("a.txt", b"a two\n"), world.propose("b.txt", b"b two\n")];
        world.write("b.txt", b"a person's edit\n", 0o644);
        let refused = world.writer.apply(SESSION, &files).unwrap_err();
        assert_unavailable(&refused, "b.txt", &WriteError::Changed.to_string());
        assert_eq!(world.bytes("a.txt"), b"a one\n");
    }

    #[test]
    fn a_link_on_the_way_is_refused() {
        let world = World::new("link-way");
        world.write("real/a.txt", b"one\n", 0o644);
        std::os::unix::fs::symlink("real", world.path("via")).unwrap();
        assert!(matches!(
            world.writer.read(Path::new("via/a.txt")),
            Err(ProposalRefused::Outside(_))
        ));

        // A directory swapped for a link after the proposal was made.
        let file = world.propose("real/a.txt", b"two\n");
        std::fs::rename(world.path("real"), world.path("moved")).unwrap();
        std::os::unix::fs::symlink("moved", world.path("real")).unwrap();
        let refused = world.writer.apply(SESSION, &[file]).unwrap_err();
        assert!(matches!(refused, ProposalRefused::Outside(_)), "{refused:?}");
        assert_eq!(world.bytes("moved/a.txt"), b"one\n");
    }

    #[test]
    fn a_link_at_the_leaf_is_refused() {
        let world = World::new("link-leaf");
        world.write("real.txt", b"one\n", 0o644);
        std::os::unix::fs::symlink("real.txt", world.path("link.txt")).unwrap();
        let refused = world.writer.read(Path::new("link.txt")).unwrap_err();
        assert!(matches!(refused, ProposalRefused::NotAFile(_)), "{refused:?}");
        assert!(sentence(&refused).contains("link.txt"));

        // The same bytes behind a link put where the file was: a write must not go through it.
        let file = world.propose("real.txt", b"two\n");
        std::fs::rename(world.path("real.txt"), world.path("kept.txt")).unwrap();
        std::os::unix::fs::symlink("kept.txt", world.path("real.txt")).unwrap();
        let refused = world.writer.apply(SESSION, &[file]).unwrap_err();
        assert_unavailable(&refused, "real.txt", &WriteError::Changed.to_string());
        assert_eq!(world.bytes("kept.txt"), b"one\n");
        assert!(world.path("real.txt").symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn a_git_path_is_refused() {
        let world = World::new("git-path");
        world.write(".git/config", b"[core]\n", 0o644);
        for rel in [".git/config", ".GIT/config"] {
            let refused = world.writer.read(Path::new(rel)).unwrap_err();
            assert!(matches!(refused, ProposalRefused::Outside(_)), "{refused:?}");
        }

        // A proposal can only have been made for a path `read` took, and `apply` does not rely on that.
        world.write("a.txt", b"[core]\n", 0o644);
        let mut file = world.propose("a.txt", b"[hooks]\n");
        file.rel = PathBuf::from(".git/config");
        let refused = world.writer.apply(SESSION, &[file]).unwrap_err();
        assert!(matches!(refused, ProposalRefused::Outside(_)), "{refused:?}");
        assert_eq!(world.bytes(".git/config"), b"[core]\n");
    }

    #[test]
    fn a_hard_linked_file_is_refused_before_any_write() {
        let world = World::new("hard-link");
        world.write("a.txt", b"a one\n", 0o644);
        world.write("b.txt", b"b one\n", 0o644);
        let files = [world.propose("a.txt", b"a two\n"), world.propose("b.txt", b"b two\n")];
        std::fs::hard_link(world.path("b.txt"), world.path("other-name.txt")).unwrap();
        let refused = world.writer.apply(SESSION, &files).unwrap_err();
        assert_unavailable(&refused, "b.txt", "hard link");
        assert_eq!(world.bytes("a.txt"), b"a one\n");
        assert_eq!(world.bytes("other-name.txt"), b"b one\n");
    }

    #[test]
    fn a_fifo_is_refused_without_opening_it() {
        let world = World::new("fifo");
        make_fifo(&world.path("pipe"));
        let reader = world.writer.clone();
        let refused = within_a_moment(move || reader.read(Path::new("pipe"))).unwrap_err();
        assert!(matches!(refused, ProposalRefused::NotAFile(_)), "{refused:?}");

        // Empty, the size of the FIFO itself, so the size alone does not refuse it.
        world.write("was-a-file", b"", 0o644);
        let file = world.propose("was-a-file", b"text\n");
        std::fs::remove_file(world.path("was-a-file")).unwrap();
        make_fifo(&world.path("was-a-file"));
        let writer = world.writer.clone();
        let refused = within_a_moment(move || writer.apply(SESSION, &[file])).unwrap_err();
        assert_unavailable(&refused, "was-a-file", &WriteError::Changed.to_string());
    }

    #[test]
    fn a_file_over_the_limit_is_refused_without_reading_it() {
        let world = World::new("too-large");
        let path = world.path("big.bin");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(PROPOSAL_MAX_BYTES + 1)
            .unwrap();
        // Unreadable, so an attempt to read it would answer something else (root reads it anyway).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = world.writer.read(Path::new("big.bin")).unwrap_err();
        assert!(matches!(refused, ProposalRefused::TooLarge(_)), "{refused:?}");
        assert!(sentence(&refused).contains("big.bin"));
    }

    #[test]
    fn a_file_grown_after_its_size_was_asked_is_refused() {
        let world = Arc::new(World::new("grown"));
        world.write("a.txt", b"one\n", 0o644);
        let during = world.clone();
        AFTER_THE_SIZE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let file = std::fs::OpenOptions::new().write(true).open(during.path("a.txt"));
                file.unwrap().set_len(2 * PROPOSAL_MAX_BYTES).unwrap();
            }));
        });
        let refused = world.writer.read(Path::new("a.txt")).unwrap_err();
        assert!(matches!(refused, ProposalRefused::TooLarge(_)), "{refused:?}");
        assert!(sentence(&refused).contains("a.txt"));
    }

    #[test]
    fn a_read_stops_one_byte_past_its_limit() {
        let world = World::new("bounded");
        world.write("a.txt", b"0123456789", 0o644);
        let dir = ProjectDir::open(&world.root).unwrap();
        let target = dir.target(Path::new("a.txt"), false).unwrap();
        let read = |limit| match read_current_within(&target, limit).unwrap() {
            Current::Regular { bytes, .. } => bytes,
            other => panic!("expected a regular file, got {other:?}"),
        };
        assert_eq!(read(4), b"01234");
        assert_eq!(read(9), b"0123456789");
        assert_eq!(read(10), b"0123456789");
    }

    #[test]
    fn a_result_over_the_limit_is_refused() {
        let world = World::new("result-too-large");
        world.write("a.txt", b"one\n", 0o644);
        let now = world.writer.read(Path::new("a.txt")).unwrap();
        let after = vec![b'a'; PROPOSAL_MAX_BYTES as usize + 1];
        let refused = world.writer.store(Path::new("a.txt"), &now, &after).unwrap_err();
        assert!(matches!(refused, ProposalRefused::TooLarge(_)), "{refused:?}");
    }

    #[test]
    fn a_failure_midway_reports_what_was_written() {
        let world = Arc::new(World::new("midway"));
        for rel in ["a.txt", "b.txt", "c.txt"] {
            world.write(rel, b"one\n", 0o644);
        }
        let files = [
            world.propose("a.txt", b"a two\n"),
            world.propose("b.txt", b"b two\n"),
            world.propose("c.txt", b"c two\n"),
        ];
        let during = world.clone();
        BETWEEN_THE_LOOKS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || during.write("b.txt", b"six\n", 0o644)));
        });
        let report = world.writer.apply(SESSION, &files).unwrap();
        assert_eq!(report.written, rels(&["a.txt"]));
        let (failed, why) = report.failed.unwrap();
        assert_eq!(failed, PathBuf::from("b.txt"));
        assert_eq!(why, WriteError::Changed.to_string());
        assert!(!why.contains("nothing was written"), "a.txt was: {why}");
        assert_eq!(report.not_tried, rels(&["c.txt"]));
        assert_eq!(world.bytes("a.txt"), b"a two\n");
        assert_eq!(world.bytes("b.txt"), b"six\n");
        assert_eq!(world.bytes("c.txt"), b"one\n");
    }

    #[test]
    fn a_file_removed_after_the_last_read_is_not_brought_back() {
        let world = World::new("removed");
        world.write("a.txt", b"one\n", 0o644);
        world.write("b.txt", b"one\n", 0o644);
        let files = [world.propose("a.txt", b"a two\n"), world.propose("b.txt", b"b two\n")];
        REMOVED_AT_THE_SPACE_CHECK.with(|path| *path.borrow_mut() = Some(world.path("a.txt")));
        let hooks = FsHooks {
            free_bytes: remove_at_the_space_check,
            fault: None,
        };
        let report = world.writer.apply_with(SESSION, &files, &hooks).unwrap();
        assert!(report.written.is_empty());
        assert_eq!(
            report.failed,
            Some((PathBuf::from("a.txt"), WriteError::Changed.to_string()))
        );
        assert_eq!(report.not_tried, rels(&["b.txt"]));
        assert!(!world.path("a.txt").exists(), "the removal stands");
        assert_eq!(
            std::fs::read_dir(&world.root).unwrap().count(),
            1,
            "no temporary is left"
        );
    }

    #[test]
    fn a_blob_collected_midway_stops_the_rest() {
        let world = Arc::new(World::new("collected"));
        for rel in ["a.txt", "b.txt", "c.txt"] {
            world.write(rel, b"one\n", 0o644);
        }
        let files = [
            world.propose("a.txt", b"a two\n"),
            world.propose("b.txt", b"b two\n"),
            world.propose("c.txt", b"c two\n"),
        ];
        let during = world.clone();
        let blob = files[1].after.clone();
        BETWEEN_THE_LOOKS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || during.prune(&blob)));
        });
        let report = world.writer.apply(SESSION, &files).unwrap();
        assert_eq!(report.written, rels(&["a.txt"]));
        let (failed, why) = report.failed.unwrap();
        assert_eq!(failed, PathBuf::from("b.txt"));
        assert!(
            why.contains("stored copy") && !why.contains("nothing was written"),
            "{why}"
        );
        assert_eq!(report.not_tried, rels(&["c.txt"]));
        assert_eq!(world.bytes("b.txt"), b"one\n");
    }

    #[test]
    fn a_write_that_fails_stops_the_rest() {
        let world = World::new("no-space");
        world.write("a.txt", b"one\n", 0o644);
        world.write("b.txt", b"one\n", 0o644);
        let files = [world.propose("a.txt", b"a two\n"), world.propose("b.txt", b"b two\n")];
        let hooks = FsHooks {
            free_bytes: |_| Ok(0),
            fault: None,
        };
        let report = world.writer.apply_with(SESSION, &files, &hooks).unwrap();
        assert!(report.written.is_empty());
        let (failed, why) = report.failed.unwrap();
        assert_eq!(failed, PathBuf::from("a.txt"));
        assert!(why.contains("not enough free space"), "{why}");
        assert_eq!(report.not_tried, rels(&["b.txt"]));
    }

    #[test]
    fn an_in_place_write_journals_and_finishes() {
        let world = World::new("in-place");
        world.write("x.txt", b"before\n", 0o644);
        if !force_in_place(&world.path("x.txt")) {
            return;
        }
        let inode = std::fs::metadata(world.path("x.txt")).unwrap().ino();
        let file = world.propose("x.txt", b"after\n");
        let journal = world.journal();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let during = (seen.clone(), journal.dir().to_path_buf());
        let hooks = FsHooks {
            fault: Some(Arc::new(move |stage: Stage| {
                if stage == Stage::JournalWritten {
                    let (seen, dir) = &during;
                    seen.lock().unwrap().extend(journal_files(dir));
                }
                Ok(())
            })),
            ..FsHooks::default()
        };
        let report = world.writer.apply_with(SESSION, &[file], &hooks).unwrap();
        assert_eq!(report.written, rels(&["x.txt"]));
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "an entry existed while the file was written"
        );
        assert_eq!(world.bytes("x.txt"), b"after\n");
        assert_eq!(std::fs::metadata(world.path("x.txt")).unwrap().ino(), inode);
        assert!(
            journal_files(journal.dir()).is_empty(),
            "the entry is gone once it is written"
        );
    }

    #[test]
    fn a_crash_left_journal_entry_names_the_session() {
        let world = World::new("crash");
        world.write("x.txt", b"before\n", 0o644);
        if !force_in_place(&world.path("x.txt")) {
            return;
        }
        let file = world.propose("x.txt", b"after\n");
        // Unwinding from inside the write leaves what a dead window leaves: the file cut short and
        // the entry unlocked.
        let hooks = FsHooks {
            fault: Some(Arc::new(|stage: Stage| {
                if stage == Stage::Truncated {
                    std::panic::resume_unwind(Box::new("the window died here"));
                }
                Ok(())
            })),
            ..FsHooks::default()
        };
        let writer = world.writer.clone();
        let files = [file.clone()];
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer.apply_with(SESSION, &files, &hooks)
        }));
        assert!(died.is_err());
        assert_eq!(world.bytes("x.txt"), b"");

        // A git child another test thread is starting holds the entry's lock for a moment.
        let journal = world.journal();
        let until = Instant::now() + Duration::from_secs(60);
        let entry = loop {
            match journal.pending().pop() {
                Some(entry) => break entry,
                None if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                None => panic!("the interrupted write left no entry"),
            }
        };
        assert_eq!(entry.session, SESSION);
        assert_eq!(entry.path, PathBuf::from("x.txt"));
        assert_eq!(entry.pre.as_deref(), Some(file.before.as_str()));
        assert_eq!(entry.intended.as_deref(), Some(file.after.as_str()));
    }

    #[test]
    fn a_session_the_journal_cannot_name_refuses_before_any_write() {
        let world = World::new("session");
        world.write("a.txt", b"a one\n", 0o644);
        world.write("x.txt", b"x one\n", 0o644);
        if !force_in_place(&world.path("x.txt")) {
            return;
        }
        let files = [world.propose("a.txt", b"a two\n"), world.propose("x.txt", b"x two\n")];
        let refused = world.writer.apply("not a session id", &files).unwrap_err();
        assert_unavailable(&refused, "x.txt", "journal");
        assert_eq!(world.bytes("a.txt"), b"a one\n");
    }

    #[test]
    fn a_busy_slot_refuses() {
        let world = World::new("busy");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        let slot = WriteSlot::open(&world.review).unwrap();
        let hold = slot.try_exclusive().unwrap();
        let files = [file];
        assert_eq!(
            world.writer.apply(SESSION, &files),
            Err(ProposalRefused::Busy(SlotBusy::Writing))
        );
        assert_eq!(world.bytes("a.txt"), b"one\n");
        drop(hold);
        assert!(world.writer.apply(SESSION, &files).is_ok(), "the slot is free again");
    }

    #[test]
    fn a_review_send_holding_the_slot_refuses() {
        let world = World::new("review-send");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        let slot = WriteSlot::open(&world.review).unwrap();
        let _hold = slot.try_shared().unwrap();
        assert_eq!(
            world.writer.apply(SESSION, &[file]),
            Err(ProposalRefused::Busy(SlotBusy::Reading))
        );
        assert_eq!(world.bytes("a.txt"), b"one\n");
    }

    #[test]
    fn no_state_directory_refuses() {
        let world = World::new("no-state");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        let writer = ProposalWriter::new(None, &world.root);
        let now = writer.read(Path::new("a.txt")).unwrap();
        assert_eq!(
            writer.store(Path::new("a.txt"), &now, b"two\n"),
            Err(ProposalRefused::NoStore)
        );
        assert_eq!(writer.texts(&file), Err(ProposalRefused::NoStore));
        assert_eq!(writer.apply(SESSION, &[file]), Err(ProposalRefused::NoStore));
        assert_eq!(world.bytes("a.txt"), b"one\n");
    }

    #[test]
    fn a_pruned_blob_says_retained() {
        let world = World::new("pruned");
        world.write("a.txt", b"a one\n", 0o644);
        world.write("b.txt", b"b one\n", 0o644);
        let files = [world.propose("a.txt", b"a two\n"), world.propose("b.txt", b"b two\n")];
        world.prune(&files[1].after);
        assert_eq!(world.writer.texts(&files[1]), Err(ProposalRefused::Retained));
        assert_eq!(world.writer.apply(SESSION, &files), Err(ProposalRefused::Retained));
        assert_eq!(world.bytes("a.txt"), b"a one\n");
    }

    #[test]
    fn blob_hunks_gives_the_unified_patch() {
        let world = World::new("hunks");
        let shadow = world.writer.spec.open().unwrap();
        let before = shadow.store_blob(b"one\ntwo\nthree").unwrap();
        let after = shadow.store_blob(b"one\n2\nthree\n").unwrap();
        let hunks = blob_hunks(&shadow, &before, &after).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_len, 3);
        assert_eq!(
            patch_text(&hunks),
            "@@ -1,3 +1,3 @@\n one\n-two\n-three\n\\ No newline at end of file\n+2\n+three\n"
        );
    }

    #[test]
    fn a_proposals_patch_reads_back_as_the_same_hunks() {
        let world = World::new("patch");
        world.write("a.txt", b"one\r\ntwo\n", 0o644);
        let file = world.propose("a.txt", b"one\r\nthree");
        let shadow = world.writer.spec.open().unwrap();
        assert_eq!(
            parse_unified(&file.hunks),
            blob_hunks(&shadow, &file.before, &file.after).unwrap()
        );
    }

    #[test]
    fn changed_bytes_always_have_hunks() {
        let world = World::new("binary");
        world.write("a.bin", b"one\0\n", 0o644);
        let now = world.writer.read(Path::new("a.bin")).unwrap();
        let refused = world.writer.store(Path::new("a.bin"), &now, b"two\0\n").unwrap_err();
        assert!(matches!(refused, ProposalRefused::NoHunks(_)), "{refused:?}");
        assert!(sentence(&refused).contains("a.bin"));
    }

    #[test]
    fn a_change_to_nothing_has_no_hunks_and_is_no_refusal() {
        let world = World::new("same");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"one\n");
        assert_eq!(file.before, file.after);
        assert_eq!(file.hunks, "");
    }

    #[test]
    fn an_absent_file_says_no_such_file() {
        let world = World::new("absent");
        for rel in ["new.rs", "no/such/dir/new.rs"] {
            let refused = world.writer.read(Path::new(rel)).unwrap_err();
            assert!(matches!(refused, ProposalRefused::NotAFile(_)), "{refused:?}");
            assert!(sentence(&refused).contains("no such file"), "{refused}");
            assert!(sentence(&refused).contains(rel), "{refused}");
        }
    }

    #[test]
    fn file_len_does_not_follow_a_link() {
        let world = World::new("file-len");
        world.write("long.txt", &[b'a'; 1000], 0o644);
        std::os::unix::fs::symlink("long.txt", world.path("link")).unwrap();
        let dir = ProjectDir::open(&world.root).unwrap();
        let len = |rel: &str| file_len(&dir.target(Path::new(rel), false).unwrap()).unwrap();
        assert_eq!(len("long.txt"), Some(1000));
        assert_eq!(len("link"), Some("long.txt".len() as u64));
        assert_eq!(len("gone.txt"), None);
        assert_eq!(len("no/parent.txt"), None);
    }

    #[test]
    fn the_texts_of_a_proposal_come_from_the_store() {
        let world = World::new("texts");
        world.write("a.txt", b"one\n", 0o644);
        let file = world.propose("a.txt", b"two\n");
        world.write("a.txt", b"something else\n", 0o644);
        assert_eq!(
            world.writer.texts(&file).unwrap(),
            ("one\n".to_owned(), "two\n".to_owned())
        );

        world.write("latin1.txt", b"caf\xe9\n", 0o644);
        let file = world.propose("latin1.txt", b"cafe\n");
        assert_unavailable(&world.writer.texts(&file).unwrap_err(), "latin1.txt", "UTF-8");
    }

    #[test]
    fn a_turn_review_hands_out_a_writer_on_its_own_store() {
        let world = World::new("from-review");
        world.write("a.txt", b"one\n", 0o644);
        let review = TurnReview::with_options(
            Some(world.review.clone()),
            &world.root,
            ReviewOptions {
                excludes: Some(None),
                ..ReviewOptions::default()
            },
        );
        let writer = review.proposal_writer();
        let now = writer.read(Path::new("a.txt")).unwrap();
        let file = writer.store(Path::new("a.txt"), &now, b"two\n").unwrap();
        assert!(world.writer.texts(&file).is_ok(), "the blobs are in the review's store");
        writer.apply(SESSION, &[file]).unwrap();
        assert_eq!(world.bytes("a.txt"), b"two\n");
    }
}
