//! Writing a project file back: the only way a revert, an undo or a recovery touches the disk.
//!
//! **Containment.** Every file is reached from the project root one directory at a time, never
//! following a symbolic link. [`ProjectDir`] holds a descriptor of the canonical root; a
//! [`Target`] holds a descriptor of the file's parent directory, reached by opening each component
//! relative to the previous one with `O_DIRECTORY|O_NOFOLLOW`, and the file's name. Everything done
//! to the file afterwards is an `*at` call on that parent descriptor, so a directory swapped for a
//! link to somewhere else, before or during the write, is refused or simply not reached: no path
//! string is ever resolved again.
//!
//! **How a file is replaced** depends on what a rename would lose. A file with one link, owned by
//! this user, with no extended attributes (an ACL is one) gets its new bytes in a temporary file in
//! the same directory, synced and renamed over it: a crash leaves the old file or the new one. Any
//! other regular file is rewritten in place, which keeps its inode, its other links, its owner and
//! its attributes; since that is not atomic, a journal entry naming its stored earlier bytes is
//! written and synced first ([`super::journal`]), and a failed write puts the earlier bytes back and
//! checks that they are there.
//!
//! Nothing here compares the disk with what the caller expected; the caller checks that right
//! before calling, and every check made here is about the file system, not the content.

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
    root: OwnedFd,
}

/// A file inside the project: a descriptor of its parent directory, reached without following a
/// link, and its name. A target whose parent does not exist (and was not to be created) has no
/// descriptor: it reads as absent, and writing to it fails.
#[derive(Debug)]
pub struct Target {
    parent: Option<OwnedFd>,
    leaf: CString,
    rel: PathBuf,
}

impl Target {
    /// The path the target was asked for, relative to the project root.
    pub fn rel(&self) -> &Path {
        &self.rel
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
}

/// What a rename would lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceWhy {
    /// The file has another hard link, which a rename would leave with the old bytes.
    HardLinks,
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
                "not enough free space ({need} bytes needed, {free} free); nothing was written"
            ),
            WriteError::Appeared => write!(f, "the file appeared since it was checked; nothing was written"),
            WriteError::Outside(why) => write!(f, "{why}"),
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
            root: unsafe { OwnedFd::from_raw_fd(fd) },
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
                        parent: None,
                        leaf,
                        rel: rel.to_path_buf(),
                    })
                }
                Err(e) => return Err(io_err(e)),
            };
        }
        Ok(Target {
            parent: Some(dir),
            leaf,
            rel: rel.to_path_buf(),
        })
    }
}

/// What `target` is on disk now, read through its parent's descriptor without following a link.
/// This is the only way a revert, an undo or a recovery reads a project file.
pub fn read_current(target: &Target) -> io::Result<Current> {
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
            let (mut file, fst) = open_leaf(parent, &target.leaf, libc::O_RDONLY, &st)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Current::Regular {
                bytes,
                mode: fst.st_mode & 0o7777,
            })
        }
        _ => Ok(Current::Other),
    }
}

/// How a regular file with `nlink` links, owned by `uid`, with `xattr_names` extended attributes is
/// replaced by a process running as `own_uid`. The first reason found wins: links, then owner, then
/// attributes.
pub fn decide_mode(nlink: u64, uid: u32, own_uid: u32, xattr_names: usize) -> WriteMode {
    if nlink > 1 {
        WriteMode::InPlace(InPlaceWhy::HardLinks)
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
/// - a regular file is replaced atomically or in place by [`decide_mode`]. An in-place write reads
///   `note.pre`'s stored bytes and opens the file before anything is journalled, so a file it cannot
///   open or a missing blob fails with nothing changed; then it journals, truncates, writes and
///   syncs, and on any failure writes the earlier bytes back and reads them again to be sure.
///
/// The mode written is always the one in `new`.
pub fn replace(
    target: &Target,
    new: &Content,
    note: &JournalNote,
    journal: &Journal,
    shadow: &Shadow,
    hooks: &FsHooks,
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
    match (new, kind) {
        (_, Some(k)) if k != libc::S_IFREG && k != libc::S_IFLNK => Err(not_regular()),
        (Content::Absent, None) => Err(WriteError::Io(format!("{shown} disappeared since it was checked"))),
        (Content::Absent, Some(_)) => {
            unlink_at(parent, &target.leaf, 0).map_err(io_err)?;
            sync_fd(parent.as_raw_fd()).map_err(io_err)
        }
        (Content::Bytes { bytes, mode }, None) => {
            let made = make_temp(parent, &target.leaf).map_err(io_err)?;
            let filled = fill_temp(&made, bytes, *mode, None, &fault);
            match filled {
                Ok(_) => link_into_place(parent, &made.name, &target.leaf, &fault, &shown),
                Err(e) => {
                    let _ = unlink_at(parent, &made.name, 0);
                    Err(io_err(e))
                }
            }
        }
        (Content::Symlink { target: link }, None) => {
            let name = make_temp_link(parent, &target.leaf, link).map_err(io_err)?;
            link_into_place(parent, &name, &target.leaf, &fault, &shown)
        }
        (Content::Symlink { target: link }, Some(_)) => {
            let name = make_temp_link(parent, &target.leaf, link).map_err(io_err)?;
            rename_into_place(parent, &name, &target.leaf).map_err(io_err)
        }
        (Content::Bytes { bytes, mode }, Some(libc::S_IFLNK)) => {
            atomic(parent, &target.leaf, bytes, *mode, None, &fault).map_err(io_err)?;
            Ok(())
        }
        (Content::Bytes { bytes, mode }, Some(_)) => {
            let st = st.expect("a kind comes from a stat");
            let (how, _) = regular_mode(parent, &target.leaf, &st).map_err(io_err)?;
            if how == WriteMode::Atomic {
                match atomic(parent, &target.leaf, bytes, *mode, Some(&st), &fault).map_err(io_err)? {
                    Renamed::Done => return Ok(()),
                    // The new file could not be given the old one's group: a rename would change
                    // who may read it, so it is rewritten in place like any file a rename would hurt.
                    Renamed::GroupWouldChange => {}
                }
            }
            in_place(parent, target, &st, bytes, *mode, note, journal, shadow, &fault)
        }
    }
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
    Ok((decide_mode(nlink, fst.st_uid, own_uid, names), fst))
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
) -> io::Result<Renamed> {
    let temp = make_temp(parent, leaf)?;
    let result = fill_temp(&temp, bytes, mode, orig, fault);
    match result {
        Ok(true) => {}
        Ok(false) => {
            let _ = unlink_at(parent, &temp.name, 0);
            return Ok(Renamed::GroupWouldChange);
        }
        Err(e) => {
            let _ = unlink_at(parent, &temp.name, 0);
            return Err(e);
        }
    }
    rename_into_place(parent, &temp.name, leaf)?;
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
    let (file, fst) = open_leaf(parent, &target.leaf, libc::O_WRONLY, st).map_err(|e| io_err(e.to_string()))?;
    let hold = journal.begin(note, shadow).map_err(|e| io_err(e.to_string()))?;

    let mut touched = false;
    let written = (|| -> io::Result<()> {
        fault(Stage::JournalWritten)?;
        touched = true;
        file.set_len(0)?;
        fault(Stage::Truncated)?;
        let mut writer = &file;
        writer.seek(SeekFrom::Start(0))?;
        write_chunks(&mut writer, bytes, fault)?;
        if mode != fst.st_mode & 0o7777 {
            // SAFETY: the descriptor is open for the call; `fchmod` takes no pointers.
            retry(|| unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })?;
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

/// Writes `pre` back into `file` (the descriptor the failed write used) with `pre_mode`, then reads
/// the file again through a fresh descriptor and checks that the bytes and the mode are `pre`'s.
fn put_back(parent: &OwnedFd, leaf: &CString, file: &File, pre: &[u8], pre_mode: u32) -> io::Result<()> {
    file.set_len(0)?;
    let mut writer = file;
    writer.seek(SeekFrom::Start(0))?;
    writer.write_all(pre)?;
    let fd = file.as_raw_fd();
    if fstat(fd)?.st_mode & 0o7777 != pre_mode {
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
    if bytes != pre || ast.st_mode & 0o7777 != pre_mode {
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
fn xattr_names(_fd: RawFd) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system's extended attributes cannot be counted",
    ))
}

/// The number of extended attribute names on the open file `fd`. A file system without extended
/// attributes has none.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn xattr_names(fd: RawFd) -> io::Result<usize> {
    loop {
        // SAFETY: a null buffer of size 0 asks only for the size; the descriptor is open.
        let size = match retry_isize(|| unsafe { flistxattr(fd, std::ptr::null_mut(), 0) }) {
            Ok(size) => size,
            Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return Ok(0),
            Err(e) => return Err(e),
        };
        if size == 0 {
            return Ok(0);
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is valid and writable for `buf.len()` bytes; the descriptor is open.
        match retry_isize(|| unsafe { flistxattr(fd, buf.as_mut_ptr().cast(), buf.len()) }) {
            Ok(got) => return Ok(buf[..got].split(|&b| b == 0).filter(|n| !n.is_empty()).count()),
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
