//! The only way discovery touches the disk.
//!
//! Every directory is opened one component at a time from `/` with
//! `openat(.., O_DIRECTORY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)`, so no component of any path opened here
//! can be a symlink; everything below an open directory is an `*at` call on its handle. A directory
//! this process may search but not read (mode `--x`) is opened for lookups only (`O_PATH` on Linux,
//! `O_SEARCH` on macOS), as the kernel itself only needs search permission to pass through it; such
//! a handle can look names up but cannot be listed. A file is looked at with
//! `fstatat(.., AT_SYMLINK_NOFOLLOW)` first, so a FIFO, socket or device is normally never opened
//! (opening a device can have side effects, opening a FIFO with no writer blocks), and then opened
//! `O_NOFOLLOW|O_NONBLOCK|O_NOCTTY` and `fstat`ed on the handle, which must still be a regular file
//! of an acceptable size before a byte is read: a name swapped for a FIFO or terminal between the
//! look and the open is opened without blocking and without becoming a controlling terminal, then
//! refused unread. A symlink is only ever read with `readlinkat`.
//!
//! The standard library's path-based calls follow the last link and block opening a FIFO with no
//! writer; one planted link must not make discovery read an account's files, and one planted FIFO
//! must not hang the thread doing it.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// Why a path was not opened or read.
#[derive(Debug)]
pub enum Refusal {
    /// Nothing is there.
    Missing,
    /// A symlink, which is never followed; `target` is what `readlinkat` says, as written.
    Symlink { target: PathBuf },
    /// Something other than a regular file where one was wanted: `"a FIFO"`, `"a socket"`,
    /// `"a device"`, `"a directory"`, ...
    NotRegular(&'static str),
    /// Something other than a directory where one was wanted.
    NotADirectory,
    /// Larger than the caller allowed; the size seen (from `fstat`, or the bytes read so far).
    TooLarge(u64),
    /// Any other failure, permission denied included.
    Io(io::Error),
}

impl Refusal {
    fn invalid(why: &'static str) -> Refusal {
        Refusal::Io(io::Error::new(io::ErrorKind::InvalidInput, why))
    }

    /// A short phrase for a person: `"a FIFO"`, `"permission denied"`, `"a link to /x"`.
    pub fn describe(&self) -> String {
        match self {
            Refusal::Missing => "missing".to_string(),
            Refusal::Symlink { target } => format!("a link to {}", target.display()),
            Refusal::NotRegular(what) => (*what).to_string(),
            Refusal::NotADirectory => "not a directory".to_string(),
            Refusal::TooLarge(len) => format!("too large ({len} bytes)"),
            Refusal::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            Refusal::Io(error) => error.to_string(),
        }
    }
}

/// What `fstatat(.., AT_SYMLINK_NOFOLLOW)` says a name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular { len: u64 },
    Dir,
    Symlink,
    Fifo,
    Socket,
    CharDevice,
    BlockDevice,
    Unknown,
}

impl FileKind {
    fn from_mode(mode: libc::mode_t, len: i64) -> FileKind {
        match mode & libc::S_IFMT {
            libc::S_IFREG => FileKind::Regular { len: len.max(0) as u64 },
            libc::S_IFDIR => FileKind::Dir,
            libc::S_IFLNK => FileKind::Symlink,
            libc::S_IFIFO => FileKind::Fifo,
            libc::S_IFSOCK => FileKind::Socket,
            libc::S_IFCHR => FileKind::CharDevice,
            libc::S_IFBLK => FileKind::BlockDevice,
            _ => FileKind::Unknown,
        }
    }

    /// How a person would name it: `"a FIFO"`, `"a socket"`, ...
    pub fn describe(self) -> &'static str {
        match self {
            FileKind::Regular { .. } => "a regular file",
            FileKind::Dir => "a directory",
            FileKind::Symlink => "a symlink",
            FileKind::Fifo => "a FIFO",
            FileKind::Socket => "a socket",
            FileKind::CharDevice | FileKind::BlockDevice => "a device",
            FileKind::Unknown => "an unknown kind of file",
        }
    }
}

/// An open directory, reached without following any link.
#[derive(Debug)]
pub struct DirHandle {
    fd: OwnedFd,
    /// False for a directory opened for lookups only, which cannot be listed.
    listable: bool,
}

/// Whose file the trust record may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Only a file owned by the effective uid.
    Me,
    /// Any owner.
    Anyone,
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
const FILE_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;

/// Opens a directory for name lookups only, which needs search permission and not read permission.
#[cfg(target_os = "linux")]
const SEARCH_ONLY_FLAGS: Option<libc::c_int> =
    Some(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
#[cfg(target_os = "macos")]
const SEARCH_ONLY_FLAGS: Option<libc::c_int> =
    Some(libc::O_SEARCH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const SEARCH_ONLY_FLAGS: Option<libc::c_int> = None;

/// The longest link target read; a longer one is refused rather than truncated.
const MAX_LINK_BYTES: usize = 4096;

/// One path component as a C string: a single name, never `.`, `..`, empty or holding a `/`, so an
/// `*at` call on it can only ever look inside the handle's own directory.
fn component(name: &OsStr) -> Result<CString, Refusal> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(Refusal::invalid("not a single path component"));
    }
    CString::new(bytes).map_err(|_| Refusal::invalid("a NUL in a path component"))
}

fn last_os_error() -> io::Error {
    io::Error::last_os_error()
}

fn fstat_fd(fd: libc::c_int) -> Result<libc::stat, Refusal> {
    // SAFETY: an all-zero `stat` is a valid value for the out-parameter `fstat` fills in.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open descriptor owned by the caller for the duration of the call, and `st`
    // is a valid, writable `stat`.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(Refusal::Io(last_os_error()));
    }
    Ok(st)
}

impl DirHandle {
    fn open_with(dirfd: libc::c_int, name: &CString, flags: libc::c_int) -> io::Result<OwnedFd> {
        // SAFETY: `dirfd` is an open directory descriptor and `name` a valid NUL-terminated string,
        // both alive for the call.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(last_os_error());
        }
        // SAFETY: `openat` just returned this descriptor, and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Opens a directory to list it, or, when only search permission is granted, for lookups only.
    fn open_at(dirfd: libc::c_int, name: &CString) -> io::Result<DirHandle> {
        match DirHandle::open_with(dirfd, name, DIR_FLAGS) {
            Ok(fd) => Ok(DirHandle { fd, listable: true }),
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => {
                let Some(flags) = SEARCH_ONLY_FLAGS else {
                    return Err(error);
                };
                let fd = DirHandle::open_with(dirfd, name, flags)?;
                // A lookup-only open does not check what it opened the way a read open does, so the
                // handle itself must be a directory (a name swapped since is refused).
                let st = fstat_fd(fd.as_raw_fd()).map_err(|_| error)?;
                if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                Ok(DirHandle { fd, listable: false })
            }
            Err(error) => Err(error),
        }
    }

    /// Opens an absolute path one component at a time from `/`, refusing any component that is a
    /// link (with its target) or not a directory, and any `..` (resolving one would mean trusting
    /// what the kernel finds behind a name this walk has not checked).
    pub fn open_absolute(path: &Path) -> Result<DirHandle, Refusal> {
        if !path.is_absolute() {
            return Err(Refusal::invalid("not an absolute path"));
        }
        let root = CString::new("/").expect("no NUL");
        // SAFETY: `root` is a valid NUL-terminated string alive for the call.
        let fd = unsafe { libc::open(root.as_ptr(), DIR_FLAGS) };
        if fd < 0 {
            return Err(Refusal::Io(last_os_error()));
        }
        // SAFETY: `open` just returned this descriptor, and nothing else owns it.
        let mut handle = DirHandle {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            listable: true,
        };
        for part in path.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => return Err(Refusal::invalid("a `..` in the path")),
                Component::Prefix(_) => return Err(Refusal::invalid("a path prefix")),
                Component::Normal(name) => handle = handle.open_dir(name)?,
            }
        }
        Ok(handle)
    }

    /// Opens a subdirectory by name, refusing a link or anything that is not a directory.
    pub fn open_dir(&self, name: &OsStr) -> Result<DirHandle, Refusal> {
        let c_name = component(name)?;
        match DirHandle::open_at(self.fd.as_raw_fd(), &c_name) {
            Ok(handle) => Ok(handle),
            Err(error) => match error.raw_os_error() {
                Some(libc::ENOENT) => Err(Refusal::Missing),
                // `O_NOFOLLOW` on a link is `ELOOP` (Linux and macOS); `O_DIRECTORY` on a link can
                // also surface as `ENOTDIR`. Either way, look at the name itself to say which.
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => match self.lstat(name)? {
                    FileKind::Symlink => Err(Refusal::Symlink {
                        target: self.readlink(name)?,
                    }),
                    _ => Err(Refusal::NotADirectory),
                },
                _ => Err(Refusal::Io(error)),
            },
        }
    }

    /// What a name is, without following it.
    pub fn lstat(&self, name: &OsStr) -> Result<FileKind, Refusal> {
        let c_name = component(name)?;
        // SAFETY: an all-zero `stat` is a valid value for the out-parameter `fstatat` fills in.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open, `c_name` is a valid NUL-terminated string and `st` a
        // valid, writable `stat`, all alive for the call.
        let rc = unsafe { libc::fstatat(self.fd.as_raw_fd(), c_name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            let error = last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOENT) => Refusal::Missing,
                _ => Refusal::Io(error),
            });
        }
        Ok(FileKind::from_mode(st.st_mode, st.st_size))
    }

    /// A link's target as written, never followed; a target longer than 4 KiB is refused.
    pub fn readlink(&self, name: &OsStr) -> Result<PathBuf, Refusal> {
        let c_name = component(name)?;
        let mut buf = vec![0u8; MAX_LINK_BYTES + 1];
        // SAFETY: the descriptor is open, `c_name` is a valid NUL-terminated string, and `buf` is a
        // writable buffer of exactly the length passed.
        let len = unsafe {
            libc::readlinkat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
            )
        };
        if len < 0 {
            let error = last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOENT) => Refusal::Missing,
                _ => Refusal::Io(error),
            });
        }
        let len = len as usize;
        if len > MAX_LINK_BYTES {
            return Err(Refusal::TooLarge(len as u64));
        }
        buf.truncate(len);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }

    /// Reads a regular file of at most `max_bytes`.
    pub fn read_file(&self, name: &OsStr, max_bytes: u64) -> Result<Vec<u8>, Refusal> {
        self.read_file_owned_by(name, max_bytes, None)
    }

    fn read_file_owned_by(&self, name: &OsStr, max_bytes: u64, owner: Option<libc::uid_t>) -> Result<Vec<u8>, Refusal> {
        let c_name = component(name)?;
        // Look before opening: a FIFO, socket or device is refused without ever being opened.
        match self.lstat(name)? {
            FileKind::Regular { len } if len > max_bytes => return Err(Refusal::TooLarge(len)),
            FileKind::Regular { .. } => {}
            FileKind::Symlink => {
                return Err(Refusal::Symlink {
                    target: self.readlink(name)?,
                })
            }
            other => return Err(Refusal::NotRegular(other.describe())),
        }
        // SAFETY: the descriptor is open and `c_name` a valid NUL-terminated string, both alive for
        // the call.
        let fd = unsafe { libc::openat(self.fd.as_raw_fd(), c_name.as_ptr(), FILE_FLAGS) };
        if fd < 0 {
            let error = last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOENT) => Refusal::Missing,
                // Swapped for a link since the look above.
                Some(libc::ELOOP) => Refusal::Symlink {
                    target: self.readlink(name)?,
                },
                // `open` on a socket (swapped in since the look above).
                Some(libc::ENXIO) => Refusal::NotRegular("a socket"),
                _ => Refusal::Io(error),
            });
        }
        // SAFETY: `openat` just returned this descriptor, and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // What was opened is what counts: the name may have been swapped since the look above.
        let st = fstat_fd(fd.as_raw_fd())?;
        match FileKind::from_mode(st.st_mode, st.st_size) {
            FileKind::Regular { len } if len > max_bytes => return Err(Refusal::TooLarge(len)),
            FileKind::Regular { .. } => {}
            other => return Err(Refusal::NotRegular(other.describe())),
        }
        if let Some(uid) = owner {
            if st.st_uid != uid {
                return Err(Refusal::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "owned by another user",
                )));
            }
        }
        // Read one byte past the limit, so a file growing under the read is still refused.
        let cap = max_bytes.saturating_add(1);
        let mut out = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        while (out.len() as u64) < cap {
            let want = chunk
                .len()
                .min((cap - out.len() as u64).min(usize::MAX as u64) as usize);
            // SAFETY: `fd` is open and `chunk` is a writable buffer of at least `want` bytes.
            let got = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast::<libc::c_void>(), want) };
            if got < 0 {
                let error = last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Refusal::Io(error));
            }
            if got == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..got as usize]);
        }
        if out.len() as u64 > max_bytes {
            return Err(Refusal::TooLarge(out.len() as u64));
        }
        Ok(out)
    }

    /// The names in this directory (not `.` or `..`), at most `max` of them, sorted by their bytes.
    /// Listed through `fdopendir` on a duplicate of this handle, so a directory swapped for a link
    /// after it was opened is never listed through the link.
    pub fn entries(&self, max: usize) -> Result<Vec<OsString>, Refusal> {
        if !self.listable {
            return Err(Refusal::Io(io::Error::from_raw_os_error(libc::EACCES)));
        }
        // SAFETY: the descriptor is open; `fcntl(F_DUPFD_CLOEXEC)` only duplicates it.
        let dup = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(Refusal::Io(last_os_error()));
        }
        // SAFETY: `dup` is a fresh descriptor of a directory; on success `fdopendir` takes ownership
        // of it, released by `closedir` below.
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            let error = last_os_error();
            // SAFETY: `fdopendir` failed, so `dup` is still ours to close.
            unsafe { libc::close(dup) };
            return Err(Refusal::Io(error));
        }
        // The duplicate shares the original's position; start from the beginning whatever an
        // earlier listing left behind.
        // SAFETY: `dir` is a valid stream returned by `fdopendir`.
        unsafe { libc::rewinddir(dir) };
        let mut names = Vec::new();
        let mut failure = None;
        while names.len() < max {
            // `readdir` reports an error only through `errno`, so clear it first.
            set_errno(0);
            // SAFETY: `dir` is a valid stream; the returned entry is read before the next call.
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                let errno = io::Error::last_os_error();
                if errno.raw_os_error().unwrap_or(0) != 0 {
                    failure = Some(errno);
                }
                break;
            }
            // SAFETY: `entry` is non-null and points at a `dirent` whose `d_name` is NUL-terminated.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        // SAFETY: `dir` is a valid stream, closed exactly once; this also closes `dup`.
        unsafe { libc::closedir(dir) };
        if let Some(error) = failure {
            return Err(Refusal::Io(error));
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(names)
    }

    /// A real directory (not a link) this process may search, as git's `access(path, X_OK)` asks.
    pub fn searchable(&self, name: &OsStr) -> bool {
        if !matches!(self.lstat(name), Ok(FileKind::Dir)) {
            return false;
        }
        let Ok(c_name) = component(name) else {
            return false;
        };
        // SAFETY: the descriptor is open and `c_name` a valid NUL-terminated string. `faccessat`
        // only checks permissions; it opens and reads nothing.
        unsafe { libc::faccessat(self.fd.as_raw_fd(), c_name.as_ptr(), libc::X_OK, 0) == 0 }
    }
}

#[cfg(target_os = "linux")]
fn set_errno(value: libc::c_int) {
    // SAFETY: `__errno_location` returns this thread's errno slot.
    unsafe { *libc::__errno_location() = value };
}

#[cfg(target_os = "macos")]
fn set_errno(value: libc::c_int) {
    // SAFETY: `__error` returns this thread's errno slot.
    unsafe { *libc::__error() = value };
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_errno(_value: libc::c_int) {}

/// Reads a regular file at an absolute path: its directory through [`DirHandle::open_absolute`],
/// then the file by [`DirHandle::read_file`]. With [`Owner::Me`] the open handle must belong to the
/// effective uid, so a file another user put in place is refused.
pub fn read_regular_nofollow(path: &Path, max_bytes: u64, owner: Owner) -> Result<Vec<u8>, Refusal> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Refusal::invalid("no file name"));
    };
    let dir = DirHandle::open_absolute(parent)?;
    let uid = match owner {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        Owner::Me => Some(unsafe { libc::geteuid() }),
        Owner::Anyone => None,
    };
    dir.read_file_owned_by(name, max_bytes, uid)
}

/// `named` joined to `base` without touching the disk: `.` is dropped, and a `..` is folded only at
/// the start of `named`, over `base` (which the caller has already opened, so it holds no link). A
/// `..` after a name is refused (`None`): folding `x/..` by text would skip a link `x` the kernel
/// would have followed. An absolute `named` replaces `base`.
pub fn normalize_lexically(base: &Path, named: &Path) -> Option<PathBuf> {
    let mut out = if named.is_absolute() {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    let mut leading = true;
    for part in named.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return None,
            Component::ParentDir if leading => {
                // At `/` there is nothing above: `/..` is `/`.
                out.pop();
            }
            Component::ParentDir => return None,
            Component::Normal(name) => {
                leading = false;
                out.push(name);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A scratch directory under its canonical path: the system temp dir may itself sit behind a
    /// link (macOS's `/tmp`, `/var`), which [`DirHandle::open_absolute`] would rightly refuse.
    pub(crate) fn canonical_scratch(label: &str) -> (ScratchDir, PathBuf) {
        let scratch = ScratchDir::new("eitri-trust", label);
        let path = scratch.canonicalize().unwrap();
        (scratch, path)
    }

    pub(crate) fn mkfifo(path: &Path) {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path alive for the call.
        assert_eq!(
            unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
            0,
            "mkfifo {}",
            path.display()
        );
    }

    #[test]
    fn read_file_reads_a_regular_file() {
        let (_s, dir) = canonical_scratch("read-ok");
        std::fs::write(dir.join("a"), b"hello").unwrap();
        let handle = DirHandle::open_absolute(&dir).unwrap();
        assert_eq!(handle.read_file(OsStr::new("a"), 5).unwrap(), b"hello");
    }

    #[test]
    fn read_file_refuses_a_symlink() {
        let (_s, dir) = canonical_scratch("read-link");
        std::fs::write(dir.join("real"), b"secret").unwrap();
        symlink(dir.join("real"), dir.join("link")).unwrap();
        let handle = DirHandle::open_absolute(&dir).unwrap();
        match handle.read_file(OsStr::new("link"), 100) {
            Err(Refusal::Symlink { target }) => assert_eq!(target, dir.join("real")),
            other => panic!("expected a symlink refusal, got {other:?}"),
        }
    }

    #[test]
    fn read_file_refuses_a_fifo_at_once() {
        let (_s, dir) = canonical_scratch("read-fifo");
        mkfifo(&dir.join("fifo"));
        let (tx, rx) = mpsc::channel();
        let path = dir.clone();
        std::thread::spawn(move || {
            let handle = DirHandle::open_absolute(&path).unwrap();
            let _ = tx.send(
                handle
                    .read_file(OsStr::new("fifo"), 100)
                    .map(|_| ())
                    .map_err(|e| e.describe()),
            );
        });
        let result = rx.recv_timeout(Duration::from_secs(2)).expect("reading a FIFO blocked");
        assert_eq!(result, Err("a FIFO".to_string()));
    }

    #[test]
    fn read_file_refuses_a_directory_and_an_oversized_file() {
        let (_s, dir) = canonical_scratch("read-dir");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("big"), vec![b'x'; 11]).unwrap();
        let handle = DirHandle::open_absolute(&dir).unwrap();
        assert!(matches!(
            handle.read_file(OsStr::new("sub"), 100),
            Err(Refusal::NotRegular("a directory"))
        ));
        assert!(matches!(
            handle.read_file(OsStr::new("big"), 10),
            Err(Refusal::TooLarge(11))
        ));
        assert_eq!(handle.read_file(OsStr::new("big"), 11).unwrap().len(), 11);
    }

    #[test]
    fn read_file_refuses_a_name_that_is_not_one_component() {
        let (_s, dir) = canonical_scratch("read-name");
        let handle = DirHandle::open_absolute(&dir).unwrap();
        for name in ["..", ".", "", "a/b"] {
            assert!(
                matches!(handle.read_file(OsStr::new(name), 10), Err(Refusal::Io(_))),
                "{name:?}"
            );
        }
    }

    #[test]
    fn open_absolute_refuses_a_symlinked_component() {
        let (_s, dir) = canonical_scratch("open-link");
        std::fs::create_dir_all(dir.join("real/inner")).unwrap();
        symlink(dir.join("real"), dir.join("link")).unwrap();
        assert!(DirHandle::open_absolute(&dir.join("real/inner")).is_ok());
        match DirHandle::open_absolute(&dir.join("link/inner")) {
            Err(Refusal::Symlink { target }) => assert_eq!(target, dir.join("real")),
            other => panic!("expected a symlink refusal, got {other:?}"),
        }
        match DirHandle::open_absolute(&dir.join("link")) {
            Err(Refusal::Symlink { .. }) => {}
            other => panic!("expected a symlink refusal, got {other:?}"),
        }
        assert!(matches!(
            DirHandle::open_absolute(&dir.join("real/../real")),
            Err(Refusal::Io(_))
        ));
        assert!(matches!(
            DirHandle::open_absolute(&dir.join("absent")),
            Err(Refusal::Missing)
        ));
        std::fs::write(dir.join("file"), b"").unwrap();
        assert!(matches!(
            DirHandle::open_absolute(&dir.join("file")),
            Err(Refusal::NotADirectory)
        ));
        assert!(DirHandle::open_absolute(Path::new("relative")).is_err());
    }

    #[test]
    fn entries_lists_sorted_names_and_stops_at_the_cap() {
        let (_s, dir) = canonical_scratch("entries");
        for name in ["c", "a", "b"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let handle = DirHandle::open_absolute(&dir).unwrap();
        let all = handle.entries(10).unwrap();
        assert_eq!(all, vec![OsString::from("a"), OsString::from("b"), OsString::from("c")]);
        // A second listing of the same handle starts over.
        assert_eq!(handle.entries(10).unwrap().len(), 3);
        assert_eq!(handle.entries(2).unwrap().len(), 2);
    }

    #[test]
    fn searchable_wants_a_real_directory() {
        let (_s, dir) = canonical_scratch("searchable");
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("f"), b"").unwrap();
        symlink(dir.join("d"), dir.join("l")).unwrap();
        let handle = DirHandle::open_absolute(&dir).unwrap();
        assert!(handle.searchable(OsStr::new("d")));
        assert!(!handle.searchable(OsStr::new("f")));
        assert!(!handle.searchable(OsStr::new("l")));
        assert!(!handle.searchable(OsStr::new("absent")));
    }

    #[test]
    fn normalize_lexically_folds_only_leading_parents() {
        let base = Path::new("/a/b/c");
        assert_eq!(normalize_lexically(base, Path::new("../..")), Some(PathBuf::from("/a")));
        assert_eq!(
            normalize_lexically(base, Path::new("../../x/./y")),
            Some(PathBuf::from("/a/x/y"))
        );
        assert_eq!(normalize_lexically(base, Path::new("x/../y")), None);
        assert_eq!(
            normalize_lexically(base, Path::new("/abs/p")),
            Some(PathBuf::from("/abs/p"))
        );
        assert_eq!(normalize_lexically(base, Path::new("/../p")), Some(PathBuf::from("/p")));
        assert_eq!(normalize_lexically(base, Path::new("/x/../p")), None);
        assert_eq!(
            normalize_lexically(Path::new("/"), Path::new("../..")),
            Some(PathBuf::from("/"))
        );
    }

    #[test]
    fn read_regular_nofollow_reads_its_own_file_and_refuses_links() {
        let (_s, dir) = canonical_scratch("regular");
        std::fs::write(dir.join("rec"), b"{}").unwrap();
        assert_eq!(read_regular_nofollow(&dir.join("rec"), 10, Owner::Me).unwrap(), b"{}");
        assert_eq!(
            read_regular_nofollow(&dir.join("rec"), 10, Owner::Anyone).unwrap(),
            b"{}"
        );
        symlink(dir.join("rec"), dir.join("link")).unwrap();
        assert!(matches!(
            read_regular_nofollow(&dir.join("link"), 10, Owner::Me),
            Err(Refusal::Symlink { .. })
        ));
    }

    #[test]
    fn read_regular_nofollow_refuses_a_foreign_file() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            // Only root can make a file another user owns; nothing to check otherwise.
            return;
        }
        let (_s, dir) = canonical_scratch("foreign");
        let path = dir.join("rec");
        std::fs::write(&path, b"{}").unwrap();
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path alive for the call.
        assert_eq!(unsafe { libc::chown(c.as_ptr(), 65534, 65534) }, 0);
        assert!(matches!(
            read_regular_nofollow(&path, 10, Owner::Me),
            Err(Refusal::Io(_))
        ));
        assert!(read_regular_nofollow(&path, 10, Owner::Anyone).is_ok());
    }

    /// Gives a directory back its owner's permissions when the test ends, so it can be removed.
    struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    #[test]
    fn a_search_only_directory_is_passed_through_but_not_listed() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            // Permissions do not hold root back; nothing to check.
            return;
        }
        let (_s, dir) = canonical_scratch("search-only");
        let x = dir.join("x");
        std::fs::create_dir_all(x.join("inner")).unwrap();
        std::fs::write(x.join("f"), b"hi").unwrap();
        std::fs::write(x.join("inner/g"), b"yo").unwrap();
        symlink(x.join("inner"), x.join("l")).unwrap();
        std::fs::create_dir(dir.join("none")).unwrap();
        let _restore_x = RestoreMode(x.clone());
        let _restore_none = RestoreMode(dir.join("none"));
        std::fs::set_permissions(&x, std::fs::Permissions::from_mode(0o100)).unwrap();
        std::fs::set_permissions(dir.join("none"), std::fs::Permissions::from_mode(0o000)).unwrap();

        // Below a search-only directory everything opens as usual.
        let inner = DirHandle::open_absolute(&x.join("inner")).unwrap();
        assert_eq!(inner.read_file(OsStr::new("g"), 10).unwrap(), b"yo");
        assert_eq!(inner.entries(10).unwrap(), vec![OsString::from("g")]);
        // The directory itself can look names up, and only listing it is refused.
        let handle = DirHandle::open_absolute(&x).unwrap();
        assert_eq!(handle.read_file(OsStr::new("f"), 10).unwrap(), b"hi");
        assert_eq!(handle.lstat(OsStr::new("inner")).unwrap(), FileKind::Dir);
        assert_eq!(handle.entries(10).unwrap_err().describe(), "permission denied");
        // A link inside it is still refused, not followed.
        match DirHandle::open_absolute(&x.join("l")) {
            Err(Refusal::Symlink { target }) => assert_eq!(target, x.join("inner")),
            other => panic!("expected a symlink refusal, got {other:?}"),
        }
        // Without search permission nothing inside can be looked at.
        let none = DirHandle::open_absolute(&dir.join("none")).unwrap();
        assert_eq!(none.lstat(OsStr::new("a")).unwrap_err().describe(), "permission denied");
        assert_eq!(none.entries(10).unwrap_err().describe(), "permission denied");
    }

    #[test]
    fn a_mode_000_file_is_refused_as_permission_denied() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_s, dir) = canonical_scratch("mode000");
        std::fs::write(dir.join("f"), b"x").unwrap();
        std::fs::set_permissions(dir.join("f"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let handle = DirHandle::open_absolute(&dir).unwrap();
        let refusal = handle.read_file(OsStr::new("f"), 10).unwrap_err();
        assert_eq!(refusal.describe(), "permission denied");
    }
}
