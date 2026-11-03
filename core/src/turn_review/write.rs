//! Writing a project file: the only way a revert, an undo, a recovery or an applied proposal
//! ([`super::proposal_write`]) touches the disk.
//!
//! **Containment.** Every file is reached from the project root one directory at a time, never
//! following a symbolic link. [`ProjectDir`] holds a descriptor of the canonical root; a
//! [`Target`] holds a descriptor of the file's parent directory, reached by opening each component
//! relative to the previous one with `O_DIRECTORY|O_NOFOLLOW`, and the file's name. Everything done
//! to the file afterwards is an `*at` call on that parent descriptor, so a directory swapped for a
//! link to somewhere else, before or during the write, is refused or simply not reached. Before
//! changing it, the parent is walked again from the root and compared with the held descriptor,
//! since a descriptor stays open even when its directory has been moved out of the project.
//!
//! **How a file is replaced** depends on what a rename would lose. A file with more than one hard
//! link is never written: any write reaches every name of it, and another name may be outside the
//! project. A file with one link, owned by this user, with no extended attributes (an ACL is one)
//! gets its new bytes in a temporary file in the same directory, synced and renamed over it: a
//! crash leaves the old file or the new one. Any other regular file (another owner, extended
//! attributes) is rewritten in place, which keeps its inode, its owner and its attributes; since
//! that is not atomic, a journal entry naming its stored earlier bytes is written and synced first
//! ([`super::journal`]), and a failed write puts the earlier bytes back and checks that they are
//! there.
//!
//! **The last look.** The caller compares the disk with what it expects right before calling; the
//! gap between that look and the write is still long enough for an editor's save or a checkout to
//! land in it. So every change to an existing path looks once more, as late as it can: right
//! before the rename, the unlink, or (in place) the truncate. A regular file is overwritten or
//! removed only when the caller named the bytes it expects there (`JournalNote::pre`) and the file
//! still holds exactly them; one the caller did not expect is left alone ([`WriteError::Changed`]).
//! A path found empty when the caller named bytes is the same refusal: writing would bring back a
//! file someone removed.

use std::ffi::CString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::journal::{Journal, JournalNote};
use super::shadow::Shadow;

type File = std::fs::File; // containment-scan: names the type; every File here wraps an openat descriptor

/// How much of an in-place rewrite is written between two looks at the fault hook.
const IN_PLACE_CHUNK: usize = 64 * 1024;

/// The longest file name most file systems take.
const NAME_MAX: usize = 255;

/// A descriptor of the canonical project root, the one place every [`Target`] is reached from.
#[derive(Debug)]
pub struct ProjectDir {
    root: Arc<OwnedFd>,
}

/// A file inside the project: a descriptor of its parent directory, reached without following a
/// link, and its name. A target whose parent does not exist (and was not to be created) has no
/// descriptor: it reads as absent, and writing to it fails.
#[derive(Debug)]
pub struct Target {
    root: Arc<OwnedFd>,
    parent: Option<OwnedFd>,
    leaf: CString,
    rel: PathBuf,
}

impl Target {
    /// The path the target was asked for, relative to the project root.
    pub fn rel(&self) -> &Path {
        &self.rel
    }

    /// Whether the project still reaches this parent without following any symbolic link.
    fn parent_is_reached(&self) -> io::Result<bool> {
        let root = ProjectDir {
            root: self.root.clone(),
        };
        let Ok(reached) = root.target(&self.rel, false) else {
            return Ok(false);
        };
        let (Some(held), Some(now)) = (&self.parent, &reached.parent) else {
            return Ok(false);
        };
        let held = fstat(held.as_raw_fd())?;
        let now = fstat(now.as_raw_fd())?;
        Ok((held.st_dev, held.st_ino) == (now.st_dev, now.st_ino))
    }
}

/// What a path is on disk now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Current {
    /// A regular file, its bytes and its permission bits.
    Regular {
        bytes: Vec<u8>,
        mode: u32,
    },
    /// A symbolic link and its target.
    Symlink {
        target: Vec<u8>,
    },
    Absent,
    /// A directory, a FIFO, a device or a socket.
    Other,
}

impl Current {
    /// The link target the caller judged, for the write's final comparison.
    pub fn link_target(&self) -> Option<&[u8]> {
        match self {
            Current::Symlink { target } => Some(target),
            _ => None,
        }
    }
}

/// What a path is to become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A regular file with exactly these bytes and these permission bits.
    Bytes { bytes: Vec<u8>, mode: u32 },
    /// A symbolic link to `target`.
    Symlink { target: Vec<u8> },
    /// Nothing: the path is removed.
    Absent,
}

/// How a regular file is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// A temporary file renamed over it.
    Atomic,
    /// Rewritten in place, journalled, for the reason given.
    InPlace(InPlaceWhy),
    /// Not written at all: the file has this many hard links, and a write through any of them
    /// would change the bytes of the others, which may live outside the project.
    HardLinked(u64),
}

/// What a rename would lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceWhy {
    /// It belongs to another user, and a renamed-in file would belong to this one.
    OtherOwner,
    /// It has extended attributes (an ACL among them), which a new file would not have.
    Xattrs,
}

/// The points of a write a test can make fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The journal entry of an in-place write exists.
    JournalWritten,
    /// An in-place write truncated the file.
    Truncated,
    /// This many bytes of the new content have been written.
    Wrote(usize),
    /// The new content was synced.
    Synced,
    /// A restore's file is complete and about to be linked into place.
    BeforeLink,
}

/// What a write asks of the file system beyond the write itself, replaceable by tests.
#[derive(Clone)]
pub struct FsHooks {
    /// The bytes free for this user on the target's file system.
    pub free_bytes: fn(&Target) -> io::Result<u64>,
    /// Called at every [`Stage`]; an error acts as the I/O error at that point. Tests only.
    pub fault: Option<Arc<dyn Fn(Stage) -> io::Result<()> + Send + Sync>>,
}

impl Default for FsHooks {
    fn default() -> Self {
        FsHooks {
            free_bytes: free_bytes_of,
            fault: None,
        }
    }
}

impl std::fmt::Debug for FsHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsHooks")
            .field("fault", &self.fault.is_some())
            .finish_non_exhaustive()
    }
}

/// Why a write did not happen, or did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Less than twice the new content's size is free.
    NoSpace { need: u64, free: u64 },
    /// A restore found something at the path when it came to put the file there.
    Appeared,
    /// The path leaves the project, or reaches it through a link.
    Outside(String),
    /// The file has other hard links, so no write to it is made: it would reach whatever else is
    /// linked to the same bytes, inside the project or not.
    HardLinked { links: u64 },
    /// The file no longer held the bytes this write expected when it came to change it; nothing was
    /// written.
    Changed,
    /// The write failed before anything was changed.
    Io(String),
    /// The write failed after the file was changed, and its earlier bytes were put back.
    RolledBack(String),
    /// The write failed after the file was changed, and its earlier bytes could not be put back;
    /// the journal entry stays, so they can be restored later.
    Broken(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::NoSpace { need, free } => write!(
                f,
                "not enough free space ({need} bytes needed, {free} free); the file was not written"
            ),
            WriteError::Appeared => write!(f, "the file appeared since it was checked; it was not written"),
            WriteError::Outside(why) => write!(f, "{why}"),
            WriteError::HardLinked { links } => write!(
                f,
                "the file has {links} hard links; writing it would change the other links too, so it was not written"
            ),
            WriteError::Changed => write!(f, "the file changed while it was being written; it was not written"),
            WriteError::Io(e) => write!(f, "the write failed: {e}"),
            WriteError::RolledBack(e) => write!(f, "the write failed and the file was put back as it was: {e}"),
            WriteError::Broken(e) => write!(
                f,
                "the write failed and the file could not be put back: {e}; its earlier bytes are kept for recovery"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

impl ProjectDir {
    /// Opens the canonical project root. Only a real directory is accepted, not a link to one.
    pub fn open(canonical_root: &Path) -> io::Result<ProjectDir> {
        let path = c_name(canonical_root.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        let fd = retry(|| unsafe { libc::open(path.as_ptr(), flags) })?; // containment-scan: the project root itself

        // SAFETY: `open` returned a new descriptor that nothing else owns.
        Ok(ProjectDir {
            root: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }),
        })
    }

    /// The file at `rel` (relative to the project root): each parent component is opened from the
    /// previous one with `O_DIRECTORY|O_NOFOLLOW`, so a link or a non-directory among them is
    /// [`WriteError::Outside`]. `rel` must be made of plain names: an absolute path, `..`, `.`, an
    /// empty component, a NUL or a `.git` component is refused the same way.
    ///
    /// A missing parent directory, with `create_missing`, is made (`mkdirat`, 0777 less the umask,
    /// as `mkdir -p` would) and then opened the same way, so a link raced into its place is still
    /// refused. Without it, the target has no parent: it reads as absent and cannot be written. A
    /// restore asks first without creating anything, and creates only for the write itself, so a
    /// restore that is refused leaves no directories behind.
    pub fn target(&self, rel: &Path, create_missing: bool) -> Result<Target, WriteError> {
        let bytes = rel.as_os_str().as_bytes();
        let shown = rel.display();
        let outside = |why: String| WriteError::Outside(format!("{shown} is not inside the project: {why}"));
        if bytes.is_empty() {
            return Err(outside("the path is empty".into()));
        }
        if bytes[0] == b'/' {
            return Err(outside("the path is absolute".into()));
        }
        if bytes.contains(&0) {
            return Err(outside("the path holds a NUL".into()));
        }
        // Split by hand: `Path::components` drops `.` and collapses `a//b`, which must be refused.
        let parts: Vec<&[u8]> = bytes.split(|&b| b == b'/').collect();
        for part in &parts {
            match *part {
                b"" => return Err(outside("it has an empty component".into())),
                b"." | b".." => return Err(outside(format!("{} is not a name", lossy(part)))),
                // A repository's own directory is never written, under any case a file system folds.
                p if p.eq_ignore_ascii_case(b".git") => return Err(outside(".git is a git directory".into())),
                _ => {}
            }
        }
        let io_err = |e: io::Error| WriteError::Io(format!("{shown}: {e}"));
        let (leaf, parents) = parts.split_last().expect("split always yields one part");
        let leaf = c_name(leaf).map_err(io_err)?;
        let mut dir = self.root.try_clone().map_err(io_err)?;
        for part in parents {
            let name = c_name(part).map_err(io_err)?;
            let linked = || outside(format!("{} is a link or not a directory", lossy(part)));
            dir = match open_dir_at(&dir, &name) {
                Ok(next) => next,
                Err(e) if is_link_or_not_dir(&e) => return Err(linked()),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) && create_missing => {
                    match make_dir_at(&dir, &name) {
                        Ok(()) => {}
                        // Made by someone else meanwhile; opening it below judges what it is.
                        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
                        Err(e) => return Err(io_err(e)),
                    }
                    let next = match open_dir_at(&dir, &name) {
                        Ok(next) => next,
                        Err(e) if is_link_or_not_dir(&e) => return Err(linked()),
                        Err(e) => return Err(io_err(e)),
                    };
                    sync_fd(dir.as_raw_fd()).map_err(io_err)?;
                    next
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                    return Ok(Target {
                        root: self.root.clone(),
                        parent: None,
                        leaf,
                        rel: rel.to_path_buf(),
                    })
                }
                Err(e) => return Err(io_err(e)),
            };
        }
        Ok(Target {
            root: self.root.clone(),
            parent: Some(dir),
            leaf,
            rel: rel.to_path_buf(),
        })
    }

    /// Whether a directory strictly between the project root and `rel`'s leaf holds a `.git`: the
    /// file is then inside a nested repository, whose contents the project's own history does not
    /// track. Directories are opened as [`target`](Self::target) opens them; one that is missing, a
    /// link or not a directory ends the walk with `false`.
    pub fn inside_nested_repository(&self, rel: &Path) -> io::Result<bool> {
        let git = c_name(b".git")?;
        let mut dir = self.root.try_clone()?;
        let mut parts: Vec<&[u8]> = rel.as_os_str().as_bytes().split(|&b| b == b'/').collect();
        parts.pop();
        for part in parts {
            dir = match open_dir_at(&dir, &c_name(part)?) {
                Ok(next) => next,
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) || is_link_or_not_dir(&e) => return Ok(false),
                Err(e) => return Err(e),
            };
            match stat_at(&dir, &git) {
                Ok(_) => return Ok(true),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }
}

/// What `target` is on disk now, read through its parent's descriptor without following a link.
/// This is the only way a revert, an undo or a recovery reads a project file.
pub fn read_current(target: &Target) -> io::Result<Current> {
    read_current_within(target, u64::MAX)
}

/// [`read_current`], reading a regular file only up to one byte past `limit`: enough for the caller
/// to tell it is longer, however far it has grown since anyone asked its size.
pub fn read_current_within(target: &Target, limit: u64) -> io::Result<Current> {
    let Some(parent) = &target.parent else {
        return Ok(Current::Absent);
    };
    let st = match stat_at(parent, &target.leaf) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Current::Absent),
        Err(e) => return Err(e),
    };
    match st.st_mode & libc::S_IFMT {
        libc::S_IFLNK => match read_link_at(parent, &target.leaf, st.st_size) {
            Ok(link) => Ok(Current::Symlink { target: link }),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(Current::Absent),
            Err(e) => Err(e),
        },
        libc::S_IFREG => {
            let (file, fst) = open_leaf(parent, &target.leaf, libc::O_RDONLY, &st)?;
            let mut bytes = Vec::new();
            file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
            Ok(Current::Regular {
                bytes,
                // `st_mode` is `u16` on macOS, `u32` on Linux; widen so `Current::Regular::mode`
                // has one type on every platform.
                mode: fst.st_mode as u32 & 0o7777,
            })
        }
        _ => Ok(Current::Other),
    }
}

/// `target`'s size in bytes without following a link; `None` when nothing is there. Opens nothing,
/// so asking about a FIFO or a device has no effect on it.
pub fn file_len(target: &Target) -> io::Result<Option<u64>> {
    let Some(parent) = &target.parent else {
        return Ok(None);
    };
    match stat_at(parent, &target.leaf) {
        Ok(st) => Ok(Some(st.st_size as u64)),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// When the file at `target` last changed, in whole milliseconds since the epoch, rounded down. `None`: nothing is
/// there. No link is followed.
pub fn file_modified_ms(target: &Target) -> io::Result<Option<u64>> {
    let Some(parent) = &target.parent else {
        return Ok(None);
    };
    match stat_at(parent, &target.leaf) {
        Ok(st) => {
            // A time before the epoch is older than any ask.
            let seconds = u64::try_from(st.st_mtime).unwrap_or(0);
            Ok(Some(seconds * 1000 + st.st_mtime_nsec as u64 / 1_000_000))
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// How a regular file with `nlink` links, owned by `uid`, with `xattr_names` extended attributes is
/// replaced by a process running as `own_uid`. The first reason found wins: links (which refuse the
/// write), then owner, then attributes.
pub fn decide_mode(nlink: u64, uid: u32, own_uid: u32, xattr_names: usize) -> WriteMode {
    if nlink > 1 {
        WriteMode::HardLinked(nlink)
    } else if uid != own_uid {
        WriteMode::InPlace(InPlaceWhy::OtherOwner)
    } else if xattr_names > 0 {
        WriteMode::InPlace(InPlaceWhy::Xattrs)
    } else {
        WriteMode::Atomic
    }
}

/// How `target` would be replaced. A symlink is always replaced as a link (atomically); anything
/// that is not a regular file or a link is an `InvalidInput` error.
pub fn write_mode(target: &Target) -> io::Result<WriteMode> {
    let parent = parent_of(target)?;
    let st = stat_at(parent, &target.leaf)?;
    match st.st_mode & libc::S_IFMT {
        libc::S_IFLNK => Ok(WriteMode::Atomic),
        libc::S_IFREG => Ok(regular_mode(parent, &target.leaf, &st)?.0),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file or a link",
        )),
    }
}

/// Makes `target` be `new`. The free space is checked first (twice the new content's size), before
/// any temporary file or journal entry exists. Then, by what is there now:
///
/// - removing a file or a link unlinks it;
/// - a file or a link put where nothing is goes through a temporary name and `linkat`, which never
///   replaces: something that appeared at the path meanwhile is [`WriteError::Appeared`] and is left
///   alone. A file system without hard links cannot restore this way, and says so as an error;
/// - a link, or a file going where a link is, is a temporary renamed over it;
/// - a regular file with another hard link is refused ([`WriteError::HardLinked`]): any write to it
///   reaches the other names too, which may be outside the project;
/// - any other regular file is replaced atomically or in place by [`decide_mode`]. An in-place write
///   reads `note.pre`'s stored bytes and opens the file before anything is journalled, so a file it
///   cannot open or a missing blob fails with nothing changed; then it journals, looks again,
///   truncates, writes and syncs, and on any failure writes the earlier bytes back and reads them
///   again to be sure.
///
/// Whatever is at the path is looked at once more right before it is replaced or removed (the last
/// look of the module's header): the same inode, kind, size and times as when this call first
/// looked, and for a regular file exactly `note.pre`'s bytes. A link must still point to
/// `judged_link`, the target the caller read before this call. A regular file when `note.pre` names
/// none, a link when it names some, or any difference, is [`WriteError::Changed`] and nothing is
/// written. So is nothing at all when `note.pre` names bytes: the caller saw a file, and a write
/// would bring back what someone removed since.
///
/// The mode written is always the one in `new`.
#[allow(clippy::too_many_arguments)]
pub fn replace(
    target: &Target,
    new: &Content,
    note: &JournalNote,
    journal: &Journal,
    shadow: &Shadow,
    hooks: &FsHooks,
    judged_link: Option<&[u8]>,
) -> Result<(), WriteError> {
    let shown = target.rel.display().to_string();
    let io_err = |e: io::Error| WriteError::Io(format!("{shown}: {e}"));
    let fault = |stage: Stage| hooks.fault.as_ref().map_or(Ok(()), |f| f(stage));
    let Some(parent) = &target.parent else {
        return Err(WriteError::Io(format!("{shown}: a parent directory is missing")));
    };
    let new_len = match new {
        Content::Bytes { bytes, .. } => Some(bytes.len()),
        Content::Symlink { target } => Some(target.len()),
        Content::Absent => None,
    };
    if let Some(len) = new_len {
        let need = 2 * len as u64;
        let free = (hooks.free_bytes)(target).map_err(io_err)?;
        if free < need {
            return Err(WriteError::NoSpace { need, free });
        }
    }
    let st = match stat_at(parent, &target.leaf) {
        Ok(st) => Some(st),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
        Err(e) => return Err(io_err(e)),
    };
    let kind = st.as_ref().map(|st| st.st_mode & libc::S_IFMT);
    let not_regular = || WriteError::Io(format!("{shown} is not a regular file or a link"));
    // Only what is replaced or removed needs its expected bytes; a path that is empty now needs none.
    let expected = match kind {
        Some(_) => expected_bytes(note, shadow).map_err(io_err)?,
        None => None,
    };
    match (new, kind) {
        (_, Some(k)) if k != libc::S_IFREG && k != libc::S_IFLNK => Err(not_regular()),
        (Content::Absent, None) => Err(WriteError::Io(format!("{shown} disappeared since it was checked"))),
        (Content::Bytes { .. } | Content::Symlink { .. }, None) if note.pre.is_some() => Err(WriteError::Changed),
        (Content::Absent, Some(_)) => {
            let st = st.expect("a kind comes from a stat");
            // An unlink cannot be taken back: the file must still be what the caller judged.
            if !still_as_judged(parent, &target.leaf, &st, expected.as_deref(), judged_link).map_err(io_err)?
                || !target.parent_is_reached().map_err(io_err)?
            {
                return Err(WriteError::Changed);
            }
            unlink_at(parent, &target.leaf, 0).map_err(io_err)?;
            sync_fd(parent.as_raw_fd()).map_err(io_err)
        }
        (Content::Bytes { bytes, mode }, None) => {
            let made = make_temp(parent, &target.leaf).map_err(io_err)?;
            let filled = fill_temp(&made, bytes, *mode, None, &fault);
            match filled {
                Ok(_) => link_into_place(target, parent, &made.name, &target.leaf, &fault, &shown),
                Err(e) => {
                    let _ = unlink_at(parent, &made.name, 0);
                    Err(io_err(e))
                }
            }
        }
        (Content::Symlink { target: link }, None) => {
            let name = make_temp_link(parent, &target.leaf, link).map_err(io_err)?;
            link_into_place(target, parent, &name, &target.leaf, &fault, &shown)
        }
        (Content::Symlink { target: link }, Some(_)) => {
            let st = st.expect("a kind comes from a stat");
            let name = make_temp_link(parent, &target.leaf, link).map_err(io_err)?;
            let unchanged = || -> io::Result<bool> {
                Ok(
                    still_as_judged(parent, &target.leaf, &st, expected.as_deref(), judged_link)?
                        && target.parent_is_reached()?,
                )
            };
            match unchanged() {
                Ok(true) => {}
                Ok(false) => {
                    let _ = unlink_at(parent, &name, 0);
                    return Err(WriteError::Changed);
                }
                Err(e) => {
                    let _ = unlink_at(parent, &name, 0);
                    return Err(io_err(e));
                }
            }
            rename_into_place(parent, &name, &target.leaf).map_err(io_err)
        }
        (Content::Bytes { bytes, mode }, Some(libc::S_IFLNK)) => {
            let st = st.expect("a kind comes from a stat");
            let unchanged = || -> io::Result<bool> {
                Ok(
                    still_as_judged(parent, &target.leaf, &st, expected.as_deref(), judged_link)?
                        && target.parent_is_reached()?,
                )
            };
            atomic(parent, &target.leaf, bytes, *mode, None, &fault, Some(&unchanged))?;
            Ok(())
        }
        (Content::Bytes { bytes, mode }, Some(_)) => {
            let st = st.expect("a kind comes from a stat");
            // A regular file the caller did not expect (it saw a link or nothing here) appeared since
            // it looked: neither a rename nor an in-place write may lose bytes nobody has seen.
            if expected.is_none() {
                return Err(WriteError::Changed);
            }
            let (how, _) = regular_mode(parent, &target.leaf, &st).map_err(io_err)?;
            match how {
                WriteMode::HardLinked(links) => return Err(WriteError::HardLinked { links }),
                WriteMode::Atomic => {
                    let unchanged = || -> io::Result<bool> {
                        Ok(
                            still_as_judged(parent, &target.leaf, &st, expected.as_deref(), judged_link)?
                                && target.parent_is_reached()?,
                        )
                    };
                    match atomic(parent, &target.leaf, bytes, *mode, Some(&st), &fault, Some(&unchanged))? {
                        Renamed::Done => return Ok(()),
                        // The new file could not be given the old one's group: a rename would change
                        // who may read it, so it is rewritten in place like any file a rename would hurt.
                        Renamed::GroupWouldChange => {}
                    }
                }
                WriteMode::InPlace(_) => {}
            }
            in_place(parent, target, &st, bytes, *mode, note, journal, shadow, &fault)
        }
    }
}

/// The stored bytes `note` says the file holds now, when it names them.
fn expected_bytes(note: &JournalNote, shadow: &Shadow) -> io::Result<Option<Vec<u8>>> {
    let Some(id) = note.pre.as_deref() else {
        return Ok(None);
    };
    match shadow.read_stored(id) {
        Ok(Some(bytes)) => Ok(Some(bytes)),
        Ok(None) => Err(io::Error::other("the stored copy of its current bytes is missing")),
        Err(e) => Err(io::Error::other(e.to_string())),
    }
}

/// Whether `leaf` is still the very entry `st` described (same inode, kind, size and times) and
/// what the caller expected there: a regular file holding exactly `expected`, or a link pointing
/// to `judged_link` when `expected` is `None`. A writer that is not Eitri (a formatter on save,
/// `git checkout`) can change a path between the last look and the rename or unlink that would lose
/// what it put there.
///
/// `expected` is `None` only when the caller saw a link or nothing at the path, so a regular file
/// found with nothing expected is one nobody looked at, and a link found with bytes expected is not
/// the file the caller judged.
fn still_as_judged(
    parent: &OwnedFd,
    leaf: &CString,
    st: &libc::stat,
    expected: Option<&[u8]>,
    judged_link: Option<&[u8]>,
) -> io::Result<bool> {
    let now = match stat_at(parent, leaf) {
        Ok(now) => now,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(false),
        Err(e) => return Err(e),
    };
    if (now.st_dev, now.st_ino) != (st.st_dev, st.st_ino)
        || now.st_mode & libc::S_IFMT != st.st_mode & libc::S_IFMT
        || now.st_size != st.st_size
        || (now.st_mtime, now.st_mtime_nsec) != (st.st_mtime, st.st_mtime_nsec)
        || (now.st_ctime, now.st_ctime_nsec) != (st.st_ctime, st.st_ctime_nsec)
    {
        return Ok(false);
    }
    match (now.st_mode & libc::S_IFMT, expected) {
        (libc::S_IFREG, Some(expected)) => {
            let mut file = match open_leaf(parent, leaf, libc::O_RDONLY, st) {
                Ok((file, _)) => file,
                // Gone, replaced or not a regular file any more: not what was judged.
                Err(_) => return Ok(false),
            };
            holds_exactly(&mut file, expected)
        }
        (libc::S_IFLNK, None) => match judged_link {
            Some(target) => {
                #[cfg(test)]
                tests::BEFORE_READ_LINK.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
                match read_link_at(parent, leaf, now.st_size) {
                    Ok(link) => Ok(link == target),
                    Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::EINVAL)) => Ok(false),
                    Err(e) => Err(e),
                }
            }
            None => Ok(false),
        },
        _ => Ok(false),
    }
}

/// Whether `target` is a regular file holding exactly `bytes` with permission bits `mode`. When it
/// is, its bytes and its directory entry are synced before this answers, through the descriptor the
/// comparison read: a caller that forgets the earlier bytes because the file already holds the new
/// ones must not do so while those new ones still sit only in the page cache.
pub(crate) fn holds_durably(target: &Target, bytes: &[u8], mode: u32) -> io::Result<bool> {
    let parent = parent_of(target)?;
    let st = match stat_at(parent, &target.leaf) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(false),
        Err(e) => return Err(e),
    };
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(false);
    }
    let (mut file, fst) = match open_leaf(parent, &target.leaf, libc::O_RDONLY, &st) {
        Ok(opened) => opened,
        Err(_) => return Ok(false),
    };
    if fst.st_mode as u32 & 0o7777 != mode || !holds_exactly(&mut file, bytes)? {
        return Ok(false);
    }
    file.sync_all()?;
    sync_fd(parent.as_raw_fd())?;
    Ok(true)
}

/// Whether the rest of `file`, from its start, is exactly `expected`; never reads more than one byte
/// past it.
fn holds_exactly(file: &mut File, expected: &[u8]) -> io::Result<bool> {
    file.seek(SeekFrom::Start(0))?;
    let mut got = Vec::with_capacity(expected.len().min(1 << 20) + 1);
    file.take(expected.len() as u64 + 1).read_to_end(&mut got)?;
    Ok(got == expected)
}

/// [`FsHooks::free_bytes`] in production: what `fstatvfs` on the parent says this user may use.
fn free_bytes_of(target: &Target) -> io::Result<u64> {
    let parent = parent_of(target)?;
    // SAFETY: an all-zero `statvfs` is a valid value for `fstatvfs` to overwrite.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the call and `st` is a valid, writable `statvfs`.
    retry(|| unsafe { libc::fstatvfs(parent.as_raw_fd(), &mut st) })?;
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

fn parent_of(target: &Target) -> io::Result<&OwnedFd> {
    target
        .parent
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "a parent directory is missing"))
}

/// The write mode of the regular file `leaf`, judged on a descriptor of the very file `st`
/// described, and that file's own `fstat`.
fn regular_mode(parent: &OwnedFd, leaf: &CString, st: &libc::stat) -> io::Result<(WriteMode, libc::stat)> {
    let (file, fst) = open_leaf(parent, leaf, libc::O_RDONLY, st)?;
    let names = xattr_names(file.as_raw_fd())?;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let own_uid = unsafe { libc::geteuid() };
    #[allow(clippy::unnecessary_cast)] // `nlink_t` is 32 bits on some targets
    let nlink = fst.st_nlink as u64;
    Ok((decide_mode(nlink, fst.st_uid, own_uid, names.len()), fst))
}

/// A temporary file being filled beside the file it will replace.
struct Temp {
    name: CString,
    file: File,
}

enum Renamed {
    Done,
    GroupWouldChange,
}

/// The temporary file's name beside `leaf`: hidden, and recognisable as Eitri's.
fn temp_name(leaf: &CString) -> io::Result<CString> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let leaf = leaf.as_bytes();
    let mut name = Vec::with_capacity(leaf.len() + 48);
    if leaf.len() + 44 > NAME_MAX {
        // A name this long would not fit with the suffix.
        name.extend_from_slice(format!(".eitri-{id}.tmp").as_bytes());
    } else {
        name.push(b'.');
        name.extend_from_slice(leaf);
        name.extend_from_slice(format!(".eitri-{id}.tmp").as_bytes());
    }
    c_name(&name)
}

fn make_temp(parent: &OwnedFd, leaf: &CString) -> io::Result<Temp> {
    let name = temp_name(leaf)?;
    let fd = open_at(
        parent,
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )?;
    Ok(Temp {
        name,
        file: fd_file(fd),
    })
}

/// Writes `bytes` into the temporary file, gives it `orig`'s group when there is an original
/// (`Ok(false)` when it cannot be given it), sets `mode` and syncs it.
fn fill_temp(
    temp: &Temp,
    bytes: &[u8],
    mode: u32,
    orig: Option<&libc::stat>,
    fault: &dyn Fn(Stage) -> io::Result<()>,
) -> io::Result<bool> {
    let mut file = &temp.file;
    write_chunks(&mut file, bytes, fault)?;
    let fd = temp.file.as_raw_fd();
    if let Some(orig) = orig {
        if fstat(fd)?.st_gid != orig.st_gid {
            // Changing the group first, since doing so clears set-id bits the mode below restores.
            // SAFETY: the descriptor is open for the call; `-1` leaves the owner as it is.
            match retry(|| unsafe { libc::fchown(fd, u32::MAX, orig.st_gid) }) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }
    // SAFETY: the descriptor is open for the call; `fchmod` takes no pointers.
    retry(|| unsafe { libc::fchmod(fd, mode as libc::mode_t) })?;
    temp.file.sync_all()?;
    fault(Stage::Synced)?;
    Ok(true)
}

/// Replaces `leaf` with a new file holding `bytes` with `mode`, through a temporary renamed over it.
/// On any failure the temporary is removed and `leaf` is as it was.
fn atomic(
    parent: &OwnedFd,
    leaf: &CString,
    bytes: &[u8],
    mode: u32,
    orig: Option<&libc::stat>,
    fault: &dyn Fn(Stage) -> io::Result<()>,
    unchanged: Option<&dyn Fn() -> io::Result<bool>>,
) -> Result<Renamed, WriteError> {
    let io_err = |e: io::Error| WriteError::Io(format!("{}: {e}", leaf.to_string_lossy()));
    let temp = make_temp(parent, leaf).map_err(io_err)?;
    let result = fill_temp(&temp, bytes, mode, orig, fault);
    match result {
        Ok(true) => {}
        Ok(false) => {
            let _ = unlink_at(parent, &temp.name, 0);
            return Ok(Renamed::GroupWouldChange);
        }
        Err(e) => {
            let _ = unlink_at(parent, &temp.name, 0);
            return Err(io_err(e));
        }
    }
    // Filling and syncing the temporary is the slow part; look at the original once more right
    // before the rename that would replace it.
    if let Some(unchanged) = unchanged {
        match unchanged() {
            Ok(true) => {}
            Ok(false) => {
                let _ = unlink_at(parent, &temp.name, 0);
                return Err(WriteError::Changed);
            }
            Err(e) => {
                let _ = unlink_at(parent, &temp.name, 0);
                return Err(io_err(e));
            }
        }
    }
    rename_into_place(parent, &temp.name, leaf).map_err(io_err)?;
    Ok(Renamed::Done)
}

/// Renames the temporary `name` over `leaf` and syncs the directory; the temporary is removed when
/// the rename fails.
fn rename_into_place(parent: &OwnedFd, name: &CString, leaf: &CString) -> io::Result<()> {
    let fd = parent.as_raw_fd();
    // SAFETY: both names are NUL-terminated strings that outlive the call; the descriptor is open.
    if let Err(e) = retry(|| unsafe { libc::renameat(fd, name.as_ptr(), fd, leaf.as_ptr()) }) {
        let _ = unlink_at(parent, name, 0);
        return Err(e);
    }
    sync_fd(fd)
}

/// A symlink to `link` under a temporary name beside `leaf`.
fn make_temp_link(parent: &OwnedFd, leaf: &CString, link: &[u8]) -> io::Result<CString> {
    let name = temp_name(leaf)?;
    let link = c_name(link)?;
    // SAFETY: both strings are NUL-terminated and outlive the call; the descriptor is open.
    retry(|| unsafe { libc::symlinkat(link.as_ptr(), parent.as_raw_fd(), name.as_ptr()) })?;
    Ok(name)
}

/// Gives the complete temporary `name` the name `leaf` with `linkat`, which never replaces what is
/// there, then removes the temporary name and syncs the directory. A path that appeared meanwhile is
/// [`WriteError::Appeared`], and is left as it is.
fn link_into_place(
    target: &Target,
    parent: &OwnedFd,
    name: &CString,
    leaf: &CString,
    fault: &dyn Fn(Stage) -> io::Result<()>,
    shown: &str,
) -> Result<(), WriteError> {
    let fd = parent.as_raw_fd();
    let fail = |e: io::Error| {
        let _ = unlink_at(parent, name, 0);
        WriteError::Io(format!("{shown}: {e}"))
    };
    fault(Stage::BeforeLink).map_err(fail)?;
    if !target.parent_is_reached().map_err(fail)? {
        let _ = unlink_at(parent, name, 0);
        return Err(WriteError::Changed);
    }
    // Flags 0: a symlink is linked as itself, not followed.
    // SAFETY: both names are NUL-terminated strings that outlive the call; the descriptor is open.
    match retry(|| unsafe { libc::linkat(fd, name.as_ptr(), fd, leaf.as_ptr(), 0) }) {
        Ok(_) => {}
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            let _ = unlink_at(parent, name, 0);
            return Err(WriteError::Appeared);
        }
        Err(e) => return Err(fail(e)),
    }
    // The file is in place under its own name; a temporary name left behind is only clutter.
    let _ = unlink_at(parent, name, 0);
    sync_fd(fd).map_err(|e| WriteError::Io(format!("{shown}: {e}")))
}

/// Rewrites the regular file `target` (described by `st`) in place with `bytes` and `mode`,
/// journalled. See [`replace`].
#[allow(clippy::too_many_arguments)]
fn in_place(
    parent: &OwnedFd,
    target: &Target,
    st: &libc::stat,
    bytes: &[u8],
    mode: u32,
    note: &JournalNote,
    journal: &Journal,
    shadow: &Shadow,
    fault: &dyn Fn(Stage) -> io::Result<()>,
) -> Result<(), WriteError> {
    let shown = target.rel.display().to_string();
    let io_err = |e: String| WriteError::Io(format!("{shown}: {e}"));
    // The bytes to put back come from the store, never from the file being written.
    let pre_id = note
        .pre
        .as_deref()
        .ok_or_else(|| io_err("there is no stored copy of its current bytes".into()))?;
    let pre = shadow
        .read_stored(pre_id)
        .map_err(|e| io_err(e.to_string()))?
        .ok_or_else(|| io_err("the stored copy of its current bytes is missing".into()))?;
    // Opened before anything is journalled: a file that cannot be opened for writing is a plain
    // failure with nothing changed, not a broken write with a recovery to offer.
    // Read and write through this one descriptor, so what is compared is what is truncated.
    let (mut file, fst) = open_leaf(parent, &target.leaf, libc::O_RDWR, st).map_err(|e| io_err(e.to_string()))?;
    // Judged again on the descriptor itself: a link made since the path was looked at is as much a
    // reason to write nothing as one that was there.
    #[allow(clippy::unnecessary_cast)] // `nlink_t` is 32 bits on some targets
    let links = fst.st_nlink as u64;
    if links > 1 {
        return Err(WriteError::HardLinked { links });
    }
    // Truncating can clear privilege metadata which this process cannot reliably restore.
    // Check ownership and group permissions before journalling or writing.
    let capability = xattr_names(file.as_raw_fd())
        .map_err(|e| io_err(e.to_string()))?
        .iter()
        .any(|name| name == b"security.capability");
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let in_group =
        euid == 0 || mode & 0o2000 == 0 || effective_user_in_group(fst.st_gid).map_err(|e| io_err(e.to_string()))?;
    can_preserve_mode(
        mode,
        fst.st_mode as u32 & 0o7777,
        fst.st_uid,
        fst.st_gid,
        euid,
        in_group,
        capability,
    )
    .map_err(io_err)?;
    let hold = journal.begin(note, mode, shadow).map_err(|e| io_err(e.to_string()))?;

    let mut touched = false;
    let mut refused: Option<WriteError> = None;
    let written = (|| -> io::Result<()> {
        fault(Stage::JournalWritten)?;
        // The journal holds `pre` as the way back, so the file must still be `pre` when it is
        // truncated; whatever a formatter or a checkout put there meanwhile is not Eitri's to lose.
        if !holds_exactly(&mut file, &pre)? {
            refused = Some(WriteError::Changed);
            return Err(io::Error::other("the file changed since it was checked"));
        }
        // Writing the journal entry takes a while, and a hard link made to the file meanwhile changes
        // none of its bytes: the links are counted again on this descriptor as the last thing before
        // the truncate, since a write now would change every name the file has.
        let now = fstat(file.as_raw_fd())?;
        #[allow(clippy::unnecessary_cast)] // `nlink_t` is 32 bits on some targets
        let links = now.st_nlink as u64;
        if links > 1 {
            refused = Some(WriteError::HardLinked { links });
            return Err(io::Error::other("the file was hard linked since it was checked"));
        }
        // And the path must still name this descriptor's file, or the write would land in a file
        // the project no longer shows while the one it does show stays as it was.
        match stat_at(parent, &target.leaf) {
            Ok(named) if (named.st_dev, named.st_ino) == (now.st_dev, now.st_ino) => {}
            Ok(_) => {
                refused = Some(WriteError::Changed);
                return Err(io::Error::other("the path names another file now"));
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                refused = Some(WriteError::Changed);
                return Err(io::Error::other("the file was removed since it was checked"));
            }
            Err(e) => return Err(e),
        }
        if !target.parent_is_reached()? {
            refused = Some(WriteError::Changed);
            return Err(io::Error::other("the parent is no longer reached from the project"));
        }
        touched = true;
        file.set_len(0)?;
        fault(Stage::Truncated)?;
        let mut writer = &file;
        writer.seek(SeekFrom::Start(0))?;
        write_chunks(&mut writer, bytes, fault)?;
        if mode != fstat(file.as_raw_fd())?.st_mode as u32 & 0o7777 {
            // SAFETY: the descriptor is open for the call; `fchmod` takes no pointers.
            retry(|| unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })?;
            #[cfg(test)]
            tests::AFTER_CHMOD.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            if mode != fstat(file.as_raw_fd())?.st_mode as u32 & 0o7777 {
                return Err(io::Error::other("the file did not keep its wanted mode"));
            }
        }
        file.sync_all()?;
        fault(Stage::Synced)?;
        Ok(())
    })();
    let error = match written {
        Ok(()) => {
            if let Err(e) = journal.remove(hold.id(), shadow) {
                eprintln!("[review] {shown} was written, but its journal entry could not be removed: {e}");
            }
            return Ok(());
        }
        Err(e) => e,
    };
    if !touched {
        if let Err(e) = journal.remove(hold.id(), shadow) {
            eprintln!("[review] the journal entry of a write that never started could not be removed: {e}");
        }
        if let Some(refusal) = refused {
            return Err(refusal);
        }
        return Err(io_err(error.to_string()));
    }
    let message = format!("{shown}: {error}");
    match put_back(parent, &target.leaf, &file, &pre, note.pre_mode) {
        Ok(()) => {
            if let Err(e) = journal.remove(hold.id(), shadow) {
                eprintln!("[review] {shown} was put back, but its journal entry could not be removed: {e}");
            }
            Err(WriteError::RolledBack(message))
        }
        Err(e) => {
            // The entry stays, unlocked once `hold` drops, for the next start to offer.
            drop(hold);
            Err(WriteError::Broken(format!(
                "{message}; putting it back failed too: {e}"
            )))
        }
    }
}

/// Whether a rewrite can keep the wanted permission bits without losing privilege metadata.
fn can_preserve_mode(
    wanted: u32,
    current: u32,
    uid: u32,
    gid: u32,
    euid: u32,
    in_group: bool,
    capability: bool,
) -> Result<(), String> {
    if capability {
        return Err("security.capability cannot be preserved by an in-place write".into());
    }
    if euid == 0 {
        return Ok(());
    }
    if uid != euid {
        if wanted != current {
            return Err("the mode of a file owned by another user cannot be changed".into());
        }
        if wanted & 0o6000 != 0 {
            return Err("set-id bits cannot be preserved on a file owned by another user".into());
        }
    }
    if wanted & 0o2000 != 0 && !in_group {
        return Err(format!(
            "set-group-id cannot be preserved without membership in group {gid}"
        ));
    }
    Ok(())
}

/// Both the effective group and supplementary groups permit retaining set-group-id.
fn effective_user_in_group(gid: libc::gid_t) -> io::Result<bool> {
    // SAFETY: `getegid` has no preconditions and cannot fail.
    if gid == unsafe { libc::getegid() } {
        return Ok(true);
    }
    loop {
        // SAFETY: size 0 with a null pointer asks only for the group count.
        let count = retry(|| unsafe { libc::getgroups(0, std::ptr::null_mut()) })?;
        if count == 0 {
            return Ok(false);
        }
        let mut groups = vec![0; count as usize];
        // SAFETY: `groups` is writable for `count` group ids.
        match retry(|| unsafe { libc::getgroups(count, groups.as_mut_ptr()) }) {
            Ok(got) => return Ok(groups[..got as usize].contains(&gid)),
            // The group list can grow between counting it and reading it.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Writes `pre` back into `file` (the descriptor the failed write used) with `pre_mode`, then reads
/// the file again through a fresh descriptor and checks that the bytes and the mode are `pre`'s.
fn put_back(parent: &OwnedFd, leaf: &CString, file: &File, pre: &[u8], pre_mode: u32) -> io::Result<()> {
    file.set_len(0)?;
    let mut writer = file;
    writer.seek(SeekFrom::Start(0))?;
    writer.write_all(pre)?;
    let fd = file.as_raw_fd();
    if fstat(fd)?.st_mode as u32 & 0o7777 != pre_mode {
        // SAFETY: the descriptor is open for the call; `fchmod` takes no pointers.
        retry(|| unsafe { libc::fchmod(fd, pre_mode as libc::mode_t) })?;
    }
    file.sync_all()?;

    let st = stat_at(parent, leaf)?;
    let held = fstat(fd)?;
    if (st.st_dev, st.st_ino) != (held.st_dev, held.st_ino) {
        return Err(io::Error::other("the file was replaced while it was being put back"));
    }
    let (mut again, ast) = open_leaf(parent, leaf, libc::O_RDONLY, &st)?;
    let mut bytes = Vec::with_capacity(pre.len());
    again.read_to_end(&mut bytes)?;
    if bytes != pre || ast.st_mode as u32 & 0o7777 != pre_mode {
        return Err(io::Error::other(
            "reading it back did not give its earlier bytes and mode",
        ));
    }
    Ok(())
}

/// Writes `bytes` in chunks, telling `fault` how much is written after each one.
fn write_chunks(out: &mut impl Write, bytes: &[u8], fault: &dyn Fn(Stage) -> io::Result<()>) -> io::Result<()> {
    let mut done = 0;
    for chunk in bytes.chunks(IN_PLACE_CHUNK) {
        out.write_all(chunk)?;
        done += chunk.len();
        fault(Stage::Wrote(done))?;
    }
    Ok(())
}

/// Elsewhere there is no call to ask, and a file whose extended attributes cannot be counted is
/// not written at all rather than risk dropping them.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn xattr_names(_fd: RawFd) -> io::Result<Vec<Vec<u8>>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system's extended attributes cannot be counted",
    ))
}

/// The extended attribute names on the open file `fd`. A file system without extended
/// attributes has none.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn xattr_names(fd: RawFd) -> io::Result<Vec<Vec<u8>>> {
    loop {
        // SAFETY: a null buffer of size 0 asks only for the size; the descriptor is open.
        let size = match retry_isize(|| unsafe { flistxattr(fd, std::ptr::null_mut(), 0) }) {
            Ok(size) => size,
            Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is valid and writable for `buf.len()` bytes; the descriptor is open.
        match retry_isize(|| unsafe { flistxattr(fd, buf.as_mut_ptr().cast(), buf.len()) }) {
            Ok(got) => {
                return Ok(buf[..got]
                    .split(|&b| b == 0)
                    .filter(|n| !n.is_empty())
                    .map(<[u8]>::to_vec)
                    .collect())
            }
            // Grown since the size was asked: ask again.
            Err(e) if e.raw_os_error() == Some(libc::ERANGE) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// `flistxattr(2)` with Linux's three arguments. Apple's takes a fourth, `options`; none of its
/// options applies to a descriptor, so it gets 0.
///
/// # Safety
/// `list` must be null with `size` 0, or valid and writable for `size` bytes.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn flistxattr(fd: RawFd, list: *mut libc::c_char, size: libc::size_t) -> libc::ssize_t {
    // SAFETY: the caller's contract is the call's.
    unsafe { libc::flistxattr(fd, list, size) }
}

/// See the Linux version above.
///
/// # Safety
/// `list` must be null with `size` 0, or valid and writable for `size` bytes.
#[cfg(target_vendor = "apple")]
unsafe fn flistxattr(fd: RawFd, list: *mut libc::c_char, size: libc::size_t) -> libc::ssize_t {
    // SAFETY: the caller's contract is the call's.
    unsafe { libc::flistxattr(fd, list, size, 0) }
}

// ---- the raw calls, each on a directory descriptor, each retried on EINTR ----

/// Wraps a descriptor this module opened with `openat` on a contained directory descriptor.
fn fd_file(fd: OwnedFd) -> File {
    File::from(fd) // containment-scan: wraps a descriptor opened by openat on a contained directory descriptor
}

fn c_name(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name holds a NUL"))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn is_link_or_not_dir(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

fn retry(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        let result = call();
        if result != -1 {
            return Ok(result);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

fn retry_isize(mut call: impl FnMut() -> isize) -> io::Result<usize> {
    loop {
        let result = call();
        if result >= 0 {
            return Ok(result as usize);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

/// `openat(dir, name, flags, mode)`, the result owned.
fn open_at(dir: &OwnedFd, name: &CString, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is open.
    let fd = retry(|| unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) })?;
    // SAFETY: `openat` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The directory `name` in `dir`, never through a link.
fn open_dir_at(dir: &OwnedFd, name: &CString) -> io::Result<OwnedFd> {
    open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

fn make_dir_at(dir: &OwnedFd, name: &CString) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is open.
    retry(|| unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) }).map(|_| ())
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`: what `name` itself is, a link not followed.
fn stat_at(dir: &OwnedFd, name: &CString) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value for `fstatat` to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated, `st` is valid and writable, the descriptor is open.
    retry(|| unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value for `fstat` to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is valid and writable; the descriptor is open.
    retry(|| unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

/// Opens the regular file `name` in `dir` (`flags` plus `O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`) and
/// checks that what was opened is a regular file and the very one `expected` described: a file
/// swapped in between the look and the open is refused, and a FIFO swapped in never blocks.
fn open_leaf(
    dir: &OwnedFd,
    name: &CString,
    flags: libc::c_int,
    expected: &libc::stat,
) -> io::Result<(File, libc::stat)> {
    let fd = open_at(
        dir,
        name,
        flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )?;
    let st = fstat(fd.as_raw_fd())?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG || (st.st_dev, st.st_ino) != (expected.st_dev, expected.st_ino) {
        return Err(io::Error::other("it was replaced while it was being opened"));
    }
    Ok((fd_file(fd), st))
}

/// The target of the link `name` in `dir`, whatever its length.
fn read_link_at(dir: &OwnedFd, name: &CString, size_hint: i64) -> io::Result<Vec<u8>> {
    let mut size = usize::try_from(size_hint).unwrap_or(0).max(63) + 1;
    loop {
        let mut buf = vec![0u8; size];
        // SAFETY: `name` is NUL-terminated, `buf` is valid and writable for `buf.len()` bytes.
        let got = retry_isize(|| unsafe {
            libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        })?;
        if got < buf.len() {
            buf.truncate(got);
            return Ok(buf);
        }
        // It filled the buffer, so it may have been cut short.
        size *= 2;
    }
}

fn unlink_at(dir: &OwnedFd, name: &CString, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is open.
    retry(|| unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(|_| ())
}

fn sync_fd(fd: RawFd) -> io::Result<()> {
    // SAFETY: the descriptor is open for the call; `fsync` takes no pointers.
    retry(|| unsafe { libc::fsync(fd) }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::os::unix::fs::PermissionsExt;

    thread_local! {
        /// A change after the link's stat was checked, before its target is read.
        pub(super) static BEFORE_READ_LINK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
        /// A permission bit lost despite a successful chmod.
        pub(super) static AFTER_CHMOD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn root_can_preserve_set_id_modes_on_another_users_file() {
        for wanted in [0o4755, 0o2755, 0o6755] {
            for current in [0o644, wanted] {
                assert!(can_preserve_mode(wanted, current, 1001, 1002, 0, false, false).is_ok());
            }
        }
    }

    #[test]
    fn file_capabilities_are_refused_for_every_effective_user() {
        for euid in [0, 1000, 1001] {
            assert!(can_preserve_mode(0o755, 0o755, 1000, 1002, euid, true, true).is_err());
        }
    }

    #[test]
    fn a_non_owner_can_only_keep_an_unchanged_unprivileged_mode() {
        assert!(can_preserve_mode(0o664, 0o664, 1001, 1002, 1000, true, false).is_ok());
        for bit in 0..12 {
            assert!(can_preserve_mode(0o664 ^ (1 << bit), 0o664, 1001, 1002, 1000, true, false).is_err());
        }
        for mode in [0o4664, 0o2664, 0o6664] {
            assert!(can_preserve_mode(mode, mode, 1001, 1002, 1000, true, false).is_err());
        }
    }

    #[test]
    fn set_group_id_requires_membership_in_the_files_group() {
        for current in [0o755, 0o2755] {
            assert!(can_preserve_mode(0o2755, current, 1000, 1002, 1000, false, false).is_err());
            assert!(can_preserve_mode(0o2755, current, 1000, 1002, 1000, true, false).is_ok());
        }
        assert!(can_preserve_mode(0o755, 0o2755, 1000, 1002, 1000, false, false).is_ok());
    }

    #[test]
    fn an_owner_can_preserve_set_user_id_and_change_permission_bits() {
        for wanted in [0o4755, 0o600, 0o1755] {
            assert!(can_preserve_mode(wanted, 0o644, 1000, 1002, 1000, false, false).is_ok());
        }
    }

    #[test]
    fn an_in_place_write_puts_back_the_file_when_chmod_does_not_keep_its_mode() {
        let scratch = ScratchDir::new("eitri-write", "mode-lost");
        let project = scratch.join("project");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("file");
        std::fs::write(&path, b"before\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let review = scratch.join("review");
        let shadow = Shadow::open_with_excludes(&review, &project, None).unwrap();
        let journal = Journal::open(&review).unwrap();
        let root = ProjectDir::open(&project).unwrap();
        let target = root.target(Path::new("file"), false).unwrap();
        let parent = parent_of(&target).unwrap();
        let st = stat_at(parent, &target.leaf).unwrap();
        let note = JournalNote {
            session: "s1".into(),
            path: PathBuf::from("file"),
            pre: Some(shadow.store_blob(b"before\n").unwrap()),
            pre_mode: 0o600,
            intended: Some(shadow.store_blob(b"after\n").unwrap()),
        };
        let changed = path.clone();
        AFTER_CHMOD.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::set_permissions(changed, std::fs::Permissions::from_mode(0o600)).unwrap();
            }));
        });
        let result = in_place(
            parent,
            &target,
            &st,
            b"after\n",
            0o640,
            &note,
            &journal,
            &shadow,
            &|_| Ok(()),
        );
        assert!(matches!(result, Err(WriteError::RolledBack(_))), "{result:?}");
        assert!(AFTER_CHMOD.with(|hook| hook.borrow().is_none()));
        assert_eq!(std::fs::read(&path).unwrap(), b"before\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o600);
        assert!(std::fs::read_dir(journal.dir()).unwrap().next().is_none());
    }

    #[test]
    fn a_link_removed_or_replaced_before_reading_its_target_is_changed() {
        for replace_with_file in [false, true] {
            let scratch = ScratchDir::new("eitri-write", "link-changed");
            let path = scratch.join("link");
            std::os::unix::fs::symlink("judged", &path).unwrap();
            let root = ProjectDir::open(&scratch).unwrap();
            let target = root.target(Path::new("link"), false).unwrap();
            let parent = parent_of(&target).unwrap();
            let st = stat_at(parent, &target.leaf).unwrap();
            BEFORE_READ_LINK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    std::fs::remove_file(&path).unwrap();
                    if replace_with_file {
                        std::fs::write(path, b"replacement\n").unwrap();
                    }
                }));
            });
            assert!(!still_as_judged(parent, &target.leaf, &st, None, Some(b"judged")).unwrap());
            assert!(BEFORE_READ_LINK.with(|hook| hook.borrow().is_none()));
        }
    }
}
