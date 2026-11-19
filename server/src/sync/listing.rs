//! Walking a project tree from its held root without following links: the listing a laptop compares with its own,
//! and the apparent sizes the sync's size check asks for.
use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

/// Whether a path (relative to the root, and whether it is a directory) is left out of a walk.
pub type Ignored<'a> = &'a dyn Fn(&str, bool) -> bool;

/// How deep a walk goes before it refuses: deeper than any project, shallow enough that the descriptors it holds
/// (one per level) stay far below a process's limit.
pub const MAX_DEPTH: usize = 256;

/// What a name is, by `lstat`; a link is never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    /// A FIFO, a socket or a device.
    Other,
    /// A name that is not UTF-8: it cannot be named on the wire, so its path is shown with replacement characters and
    /// nothing under it is walked.
    NotUtf8,
}

impl Kind {
    pub fn of(mode: libc::mode_t) -> Self {
        match mode & libc::S_IFMT {
            libc::S_IFREG => Kind::File,
            libc::S_IFDIR => Kind::Directory,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        }
    }

    /// The kind as the wire spells it.
    pub fn wire(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Directory => "directory",
            Kind::Symlink => "symlink",
            Kind::Other => "other",
            Kind::NotUtf8 => "not-utf8",
        }
    }
}

/// One name a walk reached: its path from the root, its kind and its apparent size (a link's is its target's length).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visit {
    pub rel: String,
    pub kind: Kind,
    pub size: u64,
}

/// Whether a walk goes on after a visit, or how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Go,
    Stop,
    /// The walk read as many names as it was allowed to before it could finish.
    Exhausted,
}

/// How many names a walk with `budget` may read from directories, counting the ones it leaves out (`.git`, ignored
/// names): twice the budget and some room, so a listing that fits its budget is not cut short by what it skips,
/// while a directory with millions of entries is never read whole.
pub fn read_allowance(budget: u64) -> u64 {
    budget.saturating_mul(2).saturating_add(1024)
}

/// Where a relative path's last name lives, reached from the root one directory at a time without following a link.
pub enum Reached {
    Parent(OwnedFd, CString),
    /// A directory on the way does not exist, so neither does the path.
    MissingParent,
}

/// Reaches `rel`'s parent from `root`. A component that is a link or not a directory refuses, naming it, in the words
/// a write through the turn review's `ProjectDir` uses.
pub fn reach(root: &File, rel: &str) -> Result<Reached, String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let (leaf, parents) = parts.split_last().expect("split always yields one part");
    let mut dir = dup(root.as_raw_fd()).map_err(|error| format!("{rel}: {error}"))?;
    for part in parents {
        let name = c_name(part.as_bytes()).map_err(|error| format!("{rel}: {error}"))?;
        dir = match open_dir_at(dir.as_raw_fd(), &name) {
            Ok(next) => next,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                return Err(format!(
                    "{rel} is not inside the project: {part} is a link or not a directory"
                ))
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(Reached::MissingParent),
            Err(error) => return Err(format!("{rel}: {error}")),
        };
    }
    let leaf = c_name(leaf.as_bytes()).map_err(|error| format!("{rel}: {error}"))?;
    Ok(Reached::Parent(dir, leaf))
}

/// Walks the tree under `root`, depth first in byte order of names, calling `visit` for every name the walk keeps:
/// `.git` (in any case) is never entered or listed, and what `ignored` leaves out is neither listed nor walked into.
/// A directory is walked into when its visit answers [`Flow::Go`]. Every name read from a directory costs one of
/// `allowance`; a directory with more names than are left ends the walk with [`Flow::Exhausted`] before any of them
/// is visited or sorted. A directory that cannot be read, or a tree deeper than [`MAX_DEPTH`], ends the walk with the
/// reason.
pub fn walk(
    root: &File,
    ignored: Ignored,
    allowance: &mut u64,
    deadline: Instant,
    visit: &mut dyn FnMut(&Visit) -> Flow,
) -> Result<Flow, String> {
    let dir = dup(root.as_raw_fd()).map_err(|error| format!("the root: {error}"))?;
    walk_dir(&dir, "", 0, ignored, allowance, deadline, visit)
}

/// [`walk`] from the directory `dir`, whose path from the root is `prefix` (empty for the root itself).
pub fn walk_dir(
    dir: &OwnedFd,
    prefix: &str,
    depth: usize,
    ignored: Ignored,
    allowance: &mut u64,
    deadline: Instant,
    visit: &mut dyn FnMut(&Visit) -> Flow,
) -> Result<Flow, String> {
    let shown = if prefix.is_empty() { "the root" } else { prefix };
    if Instant::now() >= deadline {
        return Ok(Flow::Exhausted);
    }
    if depth > MAX_DEPTH {
        return Err(format!("{shown} is more than {MAX_DEPTH} directories deep"));
    }
    let Some(mut names) = names_in_until(dir.as_raw_fd(), *allowance, Some(deadline))
        .map_err(|error| format!("cannot read {shown}: {error}"))?
    else {
        return Ok(Flow::Exhausted);
    };
    *allowance -= names.len() as u64;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in names {
        if Instant::now() >= deadline {
            return Ok(Flow::Exhausted);
        }
        if name.as_bytes().eq_ignore_ascii_case(b".git") {
            continue;
        }
        let c_leaf = c_name(name.as_bytes()).map_err(|error| format!("{shown}: {error}"))?;
        let st = match lstat_at(dir.as_raw_fd(), &c_leaf) {
            Ok(st) => st,
            // Removed since the directory was read: it is simply not there.
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(format!("cannot read {shown}: {error}")),
        };
        let kind = Kind::of(st.st_mode);
        let Some(text) = name.to_str() else {
            let rel = join(prefix, &name.to_string_lossy());
            let size = st.st_size as u64;
            match visit(&Visit {
                rel,
                kind: Kind::NotUtf8,
                size,
            }) {
                Flow::Go => continue,
                ended => return Ok(ended),
            }
        };
        let rel = join(prefix, text);
        if ignored(&rel, kind == Kind::Directory) {
            continue;
        }
        let size = match kind {
            Kind::File | Kind::Symlink => st.st_size as u64,
            _ => 0,
        };
        match visit(&Visit {
            rel: rel.clone(),
            kind,
            size,
        }) {
            Flow::Go => {}
            ended => return Ok(ended),
        }
        if kind == Kind::Directory {
            let child = match open_dir_at(dir.as_raw_fd(), &c_leaf) {
                Ok(child) => child,
                // Removed, or replaced by a link or a file, since it was looked at: there is nothing to walk into.
                Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR)) => {
                    continue
                }
                Err(error) => return Err(format!("cannot read {rel}: {error}")),
            };
            match walk_dir(&child, &rel, depth + 1, ignored, allowance, deadline, visit)? {
                Flow::Go => {}
                ended => return Ok(ended),
            }
        }
    }
    Ok(Flow::Go)
}

/// One step of a listing: a name, or the end of a listing that stopped at its budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listed {
    Entry(Visit),
    Truncated,
}

/// Lists the tree under `root` (see [`walk`]) through `emit`, at most `budget` names; when more remain, or the walk
/// used up its [`read_allowance`], a last [`Listed::Truncated`] says so. `emit` answering `false` (nobody reads any
/// more, or the call ran out of time) stops the walk.
pub fn list_tree(
    root: &File,
    ignored: Ignored,
    budget: u64,
    deadline: Instant,
    emit: &mut dyn FnMut(Listed) -> bool,
) -> Result<(), String> {
    let mut listed = 0u64;
    let mut allowance = read_allowance(budget);
    let mut stopped = false;
    let ended = walk(root, ignored, &mut allowance, deadline, &mut |visit| {
        if listed == budget {
            return Flow::Exhausted;
        }
        listed += 1;
        if emit(Listed::Entry(visit.clone())) {
            Flow::Go
        } else {
            stopped = true;
            Flow::Stop
        }
    })?;
    if Instant::now() >= deadline {
        return Err(format!(
            "the listing took longer than {} s",
            super::LIST_DEADLINE.as_secs()
        ));
    }
    if ended == Flow::Exhausted && !stopped {
        emit(Listed::Truncated);
    }
    Ok(())
}

/// The apparent size of each of `rels` under `root`: a file's or a link's own size, a directory's the sum of what it
/// holds (links not followed, `.git` and what `ignored` leaves out not counted), nothing there 0. Each requested path
/// costs one of `budget` whatever is there, and so does each name under it; past the budget (or the walk's
/// [`read_allowance`]) the answer stops early and says it is truncated, and the caller treats the sizes as unknown.
/// At the deadline a whole-root estimate returns its partial total. Individual path measurements refuse.
pub fn path_sizes(
    root: &File,
    rels: &[String],
    whole_root: bool,
    ignored: Ignored,
    budget: u64,
    deadline: std::time::Instant,
) -> Result<(Vec<(String, u64)>, bool), String> {
    let late = || format!("measuring took longer than {} s", super::LIST_DEADLINE.as_secs());
    let mut sizes = Vec::with_capacity(rels.len());
    let mut seen = 0u64;
    let mut allowance = read_allowance(budget);
    if whole_root {
        if !rels.is_empty() {
            return Err("whole_root requires an empty path list".into());
        }
        let mut total = 0u64;
        let ended = walk(root, ignored, &mut allowance, deadline, &mut |visit| {
            if seen == budget || std::time::Instant::now() >= deadline {
                return Flow::Exhausted;
            }
            seen += 1;
            total = total.saturating_add(visit.size);
            Flow::Go
        })?;
        return Ok((vec![(String::new(), total)], ended == Flow::Exhausted));
    }
    for rel in rels {
        super::rel(rel)?;
        if seen == budget {
            return Ok((sizes, true));
        }
        seen += 1;
        if std::time::Instant::now() >= deadline {
            return Err(late());
        }
        let (parent, leaf) = match reach(root, rel)? {
            Reached::MissingParent => {
                sizes.push((rel.clone(), 0));
                continue;
            }
            Reached::Parent(parent, leaf) => (parent, leaf),
        };
        let st = match lstat_at(parent.as_raw_fd(), &leaf) {
            Ok(st) => st,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                sizes.push((rel.clone(), 0));
                continue;
            }
            Err(error) => return Err(format!("{rel}: {error}")),
        };
        let kind = Kind::of(st.st_mode);
        if ignored(rel, kind == Kind::Directory) {
            sizes.push((rel.clone(), 0));
            continue;
        }
        let mut total = match kind {
            Kind::File | Kind::Symlink => st.st_size as u64,
            _ => 0,
        };
        if kind == Kind::Directory {
            let dir = match open_dir_at(parent.as_raw_fd(), &leaf) {
                Ok(dir) => dir,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    sizes.push((rel.clone(), 0));
                    continue;
                }
                Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                    return Err(format!("{rel} changed while it was measured"))
                }
                Err(error) => return Err(format!("cannot read {rel}: {error}")),
            };
            let ended = walk_dir(&dir, rel, 1, ignored, &mut allowance, deadline, &mut |visit| {
                if seen == budget {
                    return Flow::Exhausted;
                }
                seen += 1;
                total = total.saturating_add(visit.size);
                Flow::Go
            })?;
            if Instant::now() >= deadline {
                return Err(late());
            }
            if ended == Flow::Exhausted {
                return Ok((sizes, true));
            }
        }
        sizes.push((rel.clone(), total));
    }
    Ok((sizes, false))
}

/// Makes the directories on `rel`'s way that do not exist, each `0700` (the mode a synchronized new directory has,
/// whatever the umask) and each reached from the held root without following a link, as [`reach`] refuses them.
pub fn make_parents(root: &File, rel: &str) -> Result<(), String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut dir = dup(root.as_raw_fd()).map_err(|error| format!("{rel}: {error}"))?;
    for part in &parts[..parts.len() - 1] {
        let name = c_name(part.as_bytes()).map_err(|error| format!("{rel}: {error}"))?;
        let linked = || format!("{rel} is not inside the project: {part} is a link or not a directory");
        dir = match open_dir_at(dir.as_raw_fd(), &name) {
            Ok(next) => next,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => return Err(linked()),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is open.
                let made = retry(|| unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) });
                match &made {
                    Ok(_) => {}
                    // Made by someone else meanwhile; opening it below judges what it is.
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                    Err(error) => return Err(format!("{rel}: {error}")),
                }
                // The umask may have narrowed it; a directory made here gets exactly 0700.
                let next = match open_dir_at(dir.as_raw_fd(), &name) {
                    Ok(next) => next,
                    Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                        return Err(linked())
                    }
                    Err(error) => return Err(format!("{rel}: {error}")),
                };
                if made.is_ok() {
                    // SAFETY: fchmod takes no pointers; the descriptor is open.
                    retry(|| unsafe { libc::fchmod(next.as_raw_fd(), 0o700) })
                        .map_err(|error| format!("{rel}: {error}"))?;
                }
                next
            }
            Err(error) => return Err(format!("{rel}: {error}")),
        };
    }
    Ok(())
}

/// The ignore rule a request's patterns give, as the walks here ask it: the patterns are evaluated as Mutagen
/// evaluates the tree session's ignores, and a `.git` component in any letter case is always left out.
pub fn ignore_predicate(patterns: &[String]) -> Box<dyn Fn(&str, bool) -> bool + Send + Sync> {
    let matcher = eitri_core::remote_sync::ignores::Matcher::new(patterns);
    Box::new(move |rel: &str, is_dir: bool| {
        rel.split('/').any(|part| part.eq_ignore_ascii_case(".git")) || matcher.ignored(rel, is_dir)
    })
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The names in the directory `dir` (not `.` or `..`), read through a descriptor of their own so `dir`'s position is
/// untouched; `None` when it holds more than `cap`, found after reading at most one name past it.
pub fn names_in(dir: RawFd, cap: u64) -> io::Result<Option<Vec<OsString>>> {
    names_in_until(dir, cap, None)
}

fn names_in_until(dir: RawFd, cap: u64, deadline: Option<Instant>) -> io::Result<Option<Vec<OsString>>> {
    // `/proc/self/fd/<n>` names the very directory the descriptor holds, wherever it has been moved since.
    let held = open_at(dir, c".", libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", held.as_raw_fd()))? {
        if names.len() as u64 == cap || deadline.is_some_and(|end| Instant::now() >= end) {
            return Ok(None);
        }
        names.push(entry?.file_name());
    }
    Ok(Some(names))
}

pub fn c_name(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name holds a NUL"))
}

fn dup(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl takes no pointers; a new descriptor that nothing else owns is returned.
    let new = retry(|| unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: `new` is a fresh descriptor owned only here.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is open.
    let fd = retry(|| unsafe { libc::openat(dir, name.as_ptr(), flags) })?;
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The directory `name` in `dir`, never through a link.
pub fn open_dir_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

/// The regular file `name` in `dir` opened for reading, never through a link and never blocking on a FIFO swapped
/// in; with its own `fstat`, which callers check is a regular file.
pub fn open_file_at(dir: RawFd, name: &CStr) -> io::Result<(File, libc::stat)> {
    let fd = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    )?;
    // SAFETY: an all-zero `stat` is a valid value for fstat to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is valid and writable; the descriptor is open.
    retry(|| unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok((File::from(fd), st))
}

/// What `name` in `dir` is, a link not followed.
pub fn lstat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value for fstatat to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated, `st` is valid and writable, the descriptor is open.
    retry(|| unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

/// The target of the link `name` in `dir`, whatever its length.
pub fn read_link_at(dir: RawFd, name: &CStr) -> io::Result<Vec<u8>> {
    let mut size = 256;
    loop {
        let mut buf = vec![0u8; size];
        // SAFETY: `name` is NUL-terminated, `buf` is valid and writable for `buf.len()` bytes.
        let got = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if (got as usize) < buf.len() {
            buf.truncate(got as usize);
            return Ok(buf);
        }
        size *= 2;
    }
}

fn retry(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        let result = call();
        if result != -1 {
            return Ok(result);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}
