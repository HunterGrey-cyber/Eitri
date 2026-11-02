//! Directories and files only this user can read.
//!
//! Everything Eitri keeps under `$XDG_STATE_HOME/eitri/` -- conversation records, stored
//! histories, the prompt history, permission rules, layouts, the empty tab's mode -- used to be
//! created with the process umask's defaults: `0755` directories and `0644` files under the usual
//! umask 022. Where the path down to the state home is traversable by other accounts (Eitri is
//! the first to create `~/.local/state`, or `XDG_STATE_HOME` points somewhere shared), any local
//! user could read every prompt this user typed. The rule now:
//! directories 0700, files 0600, set exactly whatever the umask, and a directory of Eitri's own
//! with any other mode (an older build's `0755`, a `0500` a restrictive umask left) is set to
//! 0700 when Eitri next writes into it -- `chmod`, never `chown`, and only on a real directory
//! owned by this user.
//!
//! Also the one place `eitri-core` asks "is this ours?" ([`current_uid`]): its stale-instance
//! sweep must skip another user's entries in a shared `TMPDIR` before connecting to anything.
//!
//! Modes are changed with `fchmod` on a handle opened without following a symlink, never by path:
//! the handle is `fstat`ed for its owner first, so what is checked is what is changed. (This also
//! keeps the prost-setter token `wire_guard` forbids out of this crate's production code.)
//!
//! **Walking by handle, not by path.** Tightening `root/a/b/...` by re-opening each successive *path*
//! (`root`, then `root/a`, then `root/a/b`, ...) only ever protected the last component of each of
//! those opens: `O_NOFOLLOW` refuses a symlink in `root`'s own place, but the OS resolves symlinks
//! in every component that is not the last one it opens -- so a symlinked `root` left `root`
//! itself untouched (`ELOOP`) while `root/a` resolved straight through it, and a symlink at any
//! *middle* component (`root/conversations` -> elsewhere) was walked straight through the same
//! way. [`create_private_dir_all`] now tightens by walking open directory handles with `openat`
//! instead: `root` is opened once by path (still `O_NOFOLLOW`, still only its own last component),
//! and every component after that is opened relative to the *previous handle*, so no path
//! resolution ever crosses a symlinked ancestor again -- reaching a component is proof nothing
//! between `root` and it was a symlink. The first failure (root itself is a symlink or missing, or
//! any later component is) stops the walk there: nothing at or below the failure is tightened. A
//! symlinked `root` therefore means the whole tree tightens nothing at all -- new directories are
//! still created 0700 by `DirBuilder` and new files still 0600 by [`open_private`], so this is a
//! narrowing of the retroactive-tightening feature, not a loss of the baseline mode.
//!
//! **The user's configuration directory is not Eitri's.** Its files are the user's, 0644 under the
//! usual umask, and it may be a link into a dotfiles tree. So [`open_user_config_dir`] judges it and
//! never tightens it, and [`create_new_file_at`] puts one new file there, whole or not at all, without
//! ever opening a file that exists.
//!
//! **A directory another program keeps.** A home is shared with every program, and what sits in it may
//! belong to another program that sets its modes as it likes. So nothing here that takes an open
//! directory ever changes one it did not make: [`open_dir`] only opens, [`ensure_child_dir`] judges an
//! existing child and leaves it as it is or refuses it, and [`publish_private_file`] and
//! [`remove_if_same`] work on one name inside the handle they are given.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

/// Directories: owner only.
pub const PRIVATE_DIR_MODE: u32 = 0o700;
/// Files: owner read/write only.
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// This process's real user id.
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions, cannot fail and touches no memory of ours.
    unsafe { libc::getuid() }
}

/// This process's effective user id: the owner every file and directory it creates gets.
pub fn euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A real directory (a symlink is not followed) owned by this process's effective user, with no
/// group or other permission bit.
pub fn is_private_dir(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Ok(m) if m.is_dir() && m.uid() == euid() && m.mode() & 0o077 == 0)
}

/// Whether the owner and mode of the directory a private directory is created in keep anyone else
/// from replacing that private directory: owned by `euid` or root, and not group- or other-writable
/// unless sticky.
pub(crate) fn parent_is_safe(owner: u32, mode: u32, euid: u32) -> bool {
    (owner == euid || owner == 0) && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

/// Refuses `dir` as the place to create a private directory in unless [`parent_is_safe`] holds for
/// it. Whoever can rename entries in `dir` can move a validated 0700 directory away and put their
/// own, also 0700 but theirs, in its place; the sticky bit stops everyone but an entry's owner and
/// the directory's owner, both trusted by the first rule. A symlink named by `dir` is followed
/// (macOS's `/var` is one): it is the directory it leads to whose entries can be renamed. `what`
/// names what would have gone there, for the error.
pub fn require_safe_parent(dir: &Path, what: &str) -> std::io::Result<()> {
    let named =
        |error: std::io::Error| std::io::Error::new(error.kind(), format!("could not read {}: {error}", dir.display()));
    let meta = std::fs::metadata(dir).map_err(named)?;
    if meta.is_dir() && parent_is_safe(meta.uid(), meta.mode(), euid()) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "{} is not a directory of this user or root, or is writable by its group or by anyone without \
             the sticky bit, so another user could swap a directory made in it; refusing to put {what} there \
             (point XDG_RUNTIME_DIR, or TMPDIR if that is unset, at a directory of your own)",
            dir.display()
        ),
    ))
}

/// Whether `path` is itself a real directory (a symlink is not followed) owned by this user.
pub fn is_own_real_dir(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Ok(meta) if meta.is_dir() && meta.uid() == current_uid())
}

/// Sets `root` and each directory that already exists from `root` down towards `dir` to exactly
/// 0700 where its mode is anything else -- an older build's `0755`, or a `0500` a restrictive
/// umask left, in which nothing could be created -- then creates `dir` and every missing directory
/// above it, one at a time, each set to exactly 0700 before the next is made inside it, and walks
/// from `root` to `dir` once more. The walk is by open handle
/// (`openat` from the previous component), never by path, so a symlink anywhere from `root`
/// itself down to `dir`'s parent stops the walk there rather than being resolved through -- see
/// the module doc's "Walking by handle" section for why that matters and what a symlinked `root` means.
///
/// `root` is the directory Eitri owns outright, `<state home>/eitri` in production; nothing
/// above it is ever changed, because the state home and the home directory above it are shared
/// with every other program. `dir` must be `root` or inside it; otherwise only `dir` is tightened
/// (by path, the same single-component-`O_NOFOLLOW` protection [`tighten_own_dir`] always gave).
///
/// `dir`, `root`, or a directory between them that already exists and belongs to another user is
/// refused (`PermissionDenied`) before anything is tightened or written: the state home may be
/// shared, and whoever creates `eitri/...` there first would otherwise decide what this user's files
/// sit next to -- the owner of `root` can rename any directory inside it away and put one of their
/// own in its place, whatever the modes below it say.
pub fn create_private_dir_all(dir: &Path, root: &Path) -> std::io::Result<()> {
    // What already exists from `root` down is judged before anything is created or changed: a
    // directory of someone else's is refused here, not only after.
    refuse_foreign_existing(dir, root)?;
    // Then what exists is set to 0700 before anything is made inside it: an older build under a
    // restrictive umask left 0500 directories, and creating the next level in one fails.
    if let Ok(below) = dir.strip_prefix(root) {
        tighten_from_root(root, below);
    }
    create_dirs_owner_usable(dir)?;
    refuse_foreign_dirs(dir, root)?;
    match dir.strip_prefix(root) {
        Ok(below) => tighten_from_root(root, below),
        Err(_) => tighten_own_dir(dir),
    }
    Ok(())
}

/// Creates `dir` and whatever is missing above it, one directory at a time, each set to exactly 0700
/// before the next is made inside it. A recursive create leaves that to the umask, and one that takes
/// the owner's bits away (0277 gives 0500) makes the first new directory one nothing can be created
/// in. Directories that already exist are left as they are here.
///
/// Everything from the deepest directory that exists is done through open handles: each new directory
/// is made with `mkdirat` from its parent's handle, opened from that handle without following a link,
/// checked to be this user's and set with `fchmod` (a umask that takes the owner's read bit needs one
/// step more, see [`make_dir_at`]). A path is never looked up a second time, so another
/// user who can write a directory above `root` cannot swap an ancestor for a link between the steps and
/// have the mode of something else changed (or a directory made in someone else's tree). The deepest
/// existing directory itself is opened following links, as the state home may be a link of the user's.
fn create_dirs_owner_usable(dir: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut base = Path::new(".");
    for ancestor in dir.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                base = ancestor;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(ancestor),
            Err(e) => return Err(e),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(base)?;
    for path in missing.into_iter().rev() {
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} does not end in a directory name", path.display()),
            )
        })?;
        handle = make_dir_at(&handle, name, path)?;
    }
    Ok(())
}

/// Makes `name` inside the directory `parent` holds open, and returns a handle to it, set to exactly
/// 0700 when this call made it. One that appeared meanwhile is kept as it is (the walk from the root
/// tightens it afterwards), but only a real directory of this user's is accepted: a link in its place
/// is refused, never followed. `path` is only for the messages and the test hook.
///
/// A umask that takes the owner's read bit (0477 gives 0300) leaves a directory no handle can be opened
/// on, so there, and only there, the mode goes in by name from the parent's handle first. A link is not
/// followed by that either, and the handle opened after it is checked like any other.
fn make_dir_at(parent: &std::fs::File, name: &std::ffi::OsStr, path: &Path) -> std::io::Result<std::fs::File> {
    let cname = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a directory name holds a NUL"))?;
    let made = mkdir_at(parent, &cname)?;
    #[cfg(test)]
    run_after_mkdir_hook(path);
    let handle = open_after_mkdir(parent, &cname, made).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "cannot open {} as a directory of its own (a link, or removed meanwhile): {e}",
                path.display()
            ),
        )
    })?;
    refuse_foreign(path, &handle.metadata()?)?;
    if made {
        fchmod(&handle, PRIVATE_DIR_MODE)?;
    }
    Ok(handle)
}

/// `mkdirat` of `name` in `parent`: `true` when this call made it, `false` when something is already
/// at the name, whatever it is.
fn mkdir_at(parent: &std::fs::File, name: &CStr) -> std::io::Result<bool> {
    // SAFETY: `parent` is an open descriptor for the call and `name` a NUL-terminated buffer that
    // outlives it; `mkdirat` reads nothing else of ours.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), PRIVATE_DIR_MODE as libc::mode_t) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Opens `name` in `parent` as [`openat_dir`] does, after [`mkdir_at`]. A umask that takes the owner's
/// read bit leaves a directory no handle can be opened on, so only for one this call `made`, and only
/// while the name can still hold it, the mode goes in by name first.
fn open_after_mkdir(parent: &std::fs::File, name: &CStr, made: bool) -> std::io::Result<std::fs::File> {
    match openat_dir(parent, name) {
        Err(e) if made && e.raw_os_error() == Some(libc::EACCES) => match stat_at(parent, name)? {
            Some(stat) if could_be_from_mkdir(stat.st_mode) => {
                set_new_dir_mode_at(parent, name).and_then(|()| openat_dir(parent, name))
            }
            _ => Err(e),
        },
        opened => opened,
    }
}

/// Whether a directory of this `mode` can be what [`mkdir_at`] left: the umask only takes bits from
/// 0700, so a bit for group or others means something else was put at the name after the `mkdirat`.
fn could_be_from_mkdir(mode: impl Into<u32>) -> bool {
    mode.into() & 0o077 == 0
}

#[cfg(test)]
thread_local! {
    /// Called with the path of each directory just made, before anything else is done to it: a test
    /// swaps an ancestor for a link here, as another user could.
    static AFTER_MKDIR: std::cell::RefCell<Option<Box<dyn Fn(&Path)>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_after_mkdir_hook(made: &Path) {
    AFTER_MKDIR.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(made);
        }
    });
}

/// [`refuse_foreign_dirs`] for the part of the path that already exists: a directory not there yet
/// is skipped, as this runs before anything is created. Only `dir` is judged when it is not inside
/// `root`.
fn refuse_foreign_existing(dir: &Path, root: &Path) -> std::io::Result<()> {
    let inside = dir.starts_with(root);
    for candidate in dir.ancestors() {
        match std::fs::metadata(candidate) {
            Ok(meta) => refuse_foreign(candidate, &meta)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if !inside || candidate == root {
            break;
        }
    }
    Ok(())
}

/// Errors when `dir`, `root`, or any directory between them is owned by another user. A symlink
/// is looked through: what counts is the directory it leads to, which is where the files would go.
/// Only `dir` is judged when it is not inside `root`.
fn refuse_foreign_dirs(dir: &Path, root: &Path) -> std::io::Result<()> {
    let inside = dir.starts_with(root);
    for candidate in dir.ancestors() {
        refuse_foreign(candidate, &std::fs::metadata(candidate)?)?;
        if !inside || candidate == root {
            break;
        }
    }
    Ok(())
}

fn refuse_foreign(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    if meta.uid() == euid() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{} is owned by uid {}, not this user", path.display(), meta.uid()),
    ))
}

/// Walks from `root` to `below` (relative to `root`) entirely by open directory handles, tightening
/// each one reached this way to 0700 (see [`tighten_handle`]). `below`'s components must all be
/// [`Component::Normal`] (a real path stripped of `root`'s own prefix always is); any other kind,
/// or the first component that fails to open, stops the walk immediately -- nothing at or past that
/// point is touched. Reaching a component this way is proof that no symlink sat anywhere between
/// `root` and it, which a path-based re-open of `root/a/b/...` cannot promise (see the module doc).
fn tighten_from_root(root: &Path, below: &Path) {
    let Some(mut handle) = open_dir_handle(root) else {
        return;
    };
    tighten_handle(&handle);
    for component in below.components() {
        let Component::Normal(name) = component else { break };
        let Some(next) = openat_dir_handle(&handle, name) else {
            break;
        };
        tighten_handle(&next);
        handle = next;
    }
}

/// Sets `dir` to exactly 0700 when it is a real directory owned by this user whose mode is anything
/// else (a group or other bit set, or an owner bit missing). Best-effort and silent: a failure
/// leaves the files below it 0600 all the same.
///
/// This opens `dir` by *path*, so -- exactly as before the handle walk -- only `dir`'s own last path component
/// is protected from being a symlink (`O_NOFOLLOW`); an ancestor of `dir` that is itself a symlink
/// is still resolved by the OS reaching `dir`. That is unchanged and fine for this function's own
/// callers (a lone directory, or the "not under root" fallback in [`create_private_dir_all`]);
/// [`tighten_from_root`] is what closes that gap for the walk from `root` down.
pub fn tighten_own_dir(dir: &Path) {
    if let Some(handle) = open_dir_handle(dir) {
        tighten_handle(&handle);
    }
}

/// `O_NOFOLLOW`-opens `path` as a directory: a symlink in `path`'s own place, or anything else
/// wrong (missing, not a directory, no permission), is `None` rather than an error a caller must
/// handle -- every caller here treats "could not open" and "was a symlink" identically, by stopping.
fn open_dir_handle(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

/// `openat(parent, name, O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)` -- the handle-relative twin of
/// [`open_dir_handle`], and the operation that actually makes [`tighten_from_root`]'s guarantee
/// true: resolving `name` against an already-open directory handle never consults a path that a
/// symlinked ancestor could have redirected, and `O_NOFOLLOW` still refuses `name` itself being a
/// symlink. `name` must be a single path component (never `..`, never containing a `/`); every
/// caller here only ever passes one straight from `Path::components()`.
fn openat_dir_handle(parent: &std::fs::File, name: &std::ffi::OsStr) -> Option<std::fs::File> {
    let name = std::ffi::CString::new(name.as_bytes()).ok()?;
    openat_dir(parent, &name).ok()
}

/// [`openat_dir_handle`] with the reason it failed, for a caller that reports one.
fn openat_dir(parent: &std::fs::File, name: &std::ffi::CStr) -> std::io::Result<std::fs::File> {
    // SAFETY: `parent` is a valid, open descriptor for the duration of this call and stays ours;
    // `name` is a valid NUL-terminated buffer for its own lifetime. `openat` reads neither beyond
    // what those two describe.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by the `openat` call above, is a valid open descriptor, and is
    // owned by nothing else -- wrapping it in a `File` is what gives it a `Drop` that closes it.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// `fchmod`s `handle` to 0700 when it `fstat`s as a directory owned by this user whose mode is
/// anything else: a group or other bit still set, or an owner bit missing. The second is what a
/// restrictive umask leaves (0277 makes a new directory 0500, which nothing can be created in), so
/// the mode is set exactly rather than only narrowed. Shared by [`tighten_own_dir`] and
/// [`tighten_from_root`]'s walk: what is checked here is always exactly the handle that would be
/// changed, never a path re-resolved after the check.
fn tighten_handle(handle: &std::fs::File) {
    let Ok(meta) = handle.metadata() else { return };
    if !meta.is_dir() || meta.uid() != current_uid() || meta.mode() & 0o777 == PRIVATE_DIR_MODE {
        return;
    }
    let _ = fchmod(handle, PRIVATE_DIR_MODE);
}

/// Sets a directory this process just created to exactly 0700 (the umask may have taken more off),
/// through a handle opened without following a symlink.
pub(crate) fn make_dir_private(dir: &Path) -> std::io::Result<()> {
    let handle = open_dir_handle(dir)
        .ok_or_else(|| std::io::Error::other(format!("cannot open {} as a directory", dir.display())))?;
    fchmod(&handle, PRIVATE_DIR_MODE)
}

/// Sets `name` in `parent` to 0700 without following a link: for a directory this call just made and
/// cannot open.
fn set_new_dir_mode_at(parent: &std::fs::File, name: &CStr) -> std::io::Result<()> {
    // SAFETY: the descriptor is `parent`'s own and stays open for the call, and `name` is a
    // NUL-terminated buffer that outlives it; `fchmodat` reads nothing else of ours.
    let set = unsafe {
        libc::fchmodat(
            parent.as_raw_fd(),
            name.as_ptr(),
            PRIVATE_DIR_MODE as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if set == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn fchmod(handle: &std::fs::File, mode: u32) -> std::io::Result<()> {
    // SAFETY: the descriptor is `handle`'s own and stays open for the call; `fchmod` reads nothing
    // else of ours.
    if unsafe { libc::fchmod(handle.as_raw_fd(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Writes `bytes` to `path` as a 0600 file, creating or truncating it. The mode is set on the open
/// handle as well, so a file that already existed with a wider mode (a temp file a killed process
/// left behind) is narrowed too, before a byte is written.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = open_private(path)?;
    file.write_all(bytes)
}

/// Opens `path` for writing as a 0600 file, creating or truncating it (see [`write_private`]). A
/// symlink in `path`'s place is refused (`O_NOFOLLOW`), never written through.
///
/// What was opened must be a regular file owned by this user, or the open fails with
/// `PermissionDenied`: a FIFO someone planted would otherwise block this `open` for good, and a
/// file of another user is not ours to truncate. `O_NONBLOCK` makes a FIFO without a reader fail
/// at once, and the type and owner are read from the open handle, so what is checked is what is
/// written. Truncation happens after that check, never as part of the open.
pub fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    fchmod(&file, PRIVATE_FILE_MODE)?;
    file.set_len(0)?;
    Ok(file)
}

/// Makes `name` in `dir` as a new file, set to exactly 0600 on the handle whatever the umask: a name that already exists,
/// a link included, is refused (`O_CREAT | O_EXCL`). Takes the name as a C string and allocates nothing on the way, so
/// a panic hook can call it.
pub fn create_private_file_at(dir: BorrowedFd<'_>, name: &CStr) -> std::io::Result<std::fs::File> {
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `name` is a NUL-terminated string that outlives the call, and `dir` is an open descriptor.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, PRIVATE_FILE_MODE as libc::c_uint) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` just returned this descriptor and nothing else owns it.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: `file` is open for the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), PRIVATE_FILE_MODE as libc::mode_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// Reads `path` whole as UTF-8, but only when it is a regular file owned by this user; anything
/// else is refused with `PermissionDenied` before a byte is read (a missing file is still
/// `NotFound`). Every loader of Eitri's own state reads through this: those files decide what is
/// answered without a card and how a window comes back, so one that another user put in place --
/// in a state home they could write to -- must not count, and a FIFO in a file's place must not
/// block a launch. `O_NONBLOCK` makes opening a FIFO return at once, and the type and owner are
/// read from the open handle, so what is checked is what is read.
pub fn read_private_to_string(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// A lock file of this user's whose exclusive `flock` stands for one store file's read-modify-write.
/// The lock, when [`try_lock`](Self::try_lock) took it, is held until this is dropped: closing the
/// descriptor releases it, and so does the process ending, however it ends, so a crashed window never
/// leaves a store locked.
///
/// The lock is a file beside the store, never the store itself: a store is replaced by a rename, and a
/// lock on the inode a writer opened would not exclude one that opened the file renamed over it.
/// `flock` belongs to the open file description, so two opens of one lock file exclude each other even
/// inside one process (two threads, each with its own `PrivateLock`); over NFS Linux takes it as a
/// byte-range lock, which excludes other processes only, so there each store must keep a single writer
/// per process (each window has one per store). A child forked while it is held
/// shares the description until its `exec` closes it (`O_CLOEXEC`), so it may hold a moment longer.
pub struct PrivateLock {
    file: std::fs::File,
}

impl PrivateLock {
    /// Opens `path`, creating it 0600, without truncating it. As with [`open_private`], a symlink is
    /// refused (`O_NOFOLLOW`), a FIFO cannot hold the open up (`O_NONBLOCK`), and anything but a regular
    /// file of this user is refused before it is locked or changed. Opened for writing as well, though
    /// nothing is written: on NFS, Linux takes an `flock` as a byte-range lock, and an exclusive one on a
    /// descriptor open only for reading fails.
    ///
    /// A missing lock file is made under a name of this process's own, set to 0600 there, and only then
    /// linked into place, so it never appears with another mode. Created in place, it would be what the
    /// umask makes it until its mode is set -- 0400 under a umask such as 0277 -- and another window
    /// opening it for writing in that moment would be refused, and its write with it. When another
    /// window links its own first, that one is opened. A file system without hard links gets the file
    /// created in place.
    pub fn open(path: &Path) -> std::io::Result<PrivateLock> {
        // Each round either opens what is there or puts a file there; only a lock file removed again
        // between the two, every time, runs out of rounds.
        for _ in 0..8 {
            match open_lock_file(path, false) {
                Ok(file) => return PrivateLock::checked(file, path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            if let Some(file) = create_lock_file(path)? {
                return PrivateLock::checked(file, path);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} kept disappearing while it was opened", path.display()),
        ))
    }

    fn checked(file: std::fs::File, path: &Path) -> std::io::Result<PrivateLock> {
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != euid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a regular file of this user", path.display()),
            ));
        }
        fchmod(&file, PRIVATE_FILE_MODE)?;
        Ok(PrivateLock { file })
    }

    /// One try at the exclusive lock, never waiting: `true` when this now holds it (or already did),
    /// `false` when another open of the file holds it.
    pub fn try_lock(&self) -> std::io::Result<bool> {
        loop {
            // SAFETY: the descriptor is `self.file`'s own and open for the call; `flock` takes no pointers.
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(true);
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) => return Ok(false),
                _ => return Err(err),
            }
        }
    }
}

fn open_lock_file(path: &Path, create: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Numbers this process's temporary lock files, so two threads making one at once never share a name.
static LOCK_FILE_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Puts a 0600 lock file at `path` (see [`PrivateLock::open`]): the file made, or `None` when another
/// window's was linked there first.
fn create_lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let mut name = path.as_os_str().to_os_string();
    let serial = LOCK_FILE_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    name.push(format!(".{}.{serial}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(name);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&tmp)
    {
        Ok(file) => file,
        // Left by a process that ended with this one's id; the next round takes another name.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e),
    };
    #[cfg(test)]
    lock_test_hooks::after_create();
    let linked = fchmod(&file, PRIVATE_FILE_MODE).and_then(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS)) => {
            // No hard links here: made in place, as the only way left.
            drop(file);
            open_lock_file(path, true).map(Some)
        }
        Err(e) => Err(e),
    }
}

/// A hook a test sets on its own thread to stop [`PrivateLock::open`] right after the open that creates
/// a lock file and before its mode is set, so a second open can be made at that moment.
#[cfg(test)]
mod lock_test_hooks {
    use std::cell::RefCell;

    thread_local! {
        static AFTER_CREATE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn set_after_create(hook: impl FnOnce() + 'static) {
        AFTER_CREATE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn after_create() {
        let hook = AFTER_CREATE.with(|h| h.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Reads private UTF-8 state within a byte cap, checked before reading and while the file may grow.
pub fn read_private_to_string_capped(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    read_capped(file, path, max_bytes)
}

/// [`read_private_to_string_capped`] for a name another process could have put in place: a symbolic link there
/// is refused (`O_NOFOLLOW`), never followed, so what is read is the file at that name or nothing.
pub fn read_private_nofollow_capped(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    read_capped(open_nofollow(path)?, path, max_bytes)
}

/// [`read_private_nofollow_capped`] with no UTF-8 step, for a record that keeps a path's raw bytes.
pub fn read_private_nofollow_bytes_capped(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    read_capped_bytes(open_nofollow(path)?, path, max_bytes)
}

fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// [`read_capped_bytes`] as UTF-8.
fn read_capped(file: std::fs::File, path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let bytes = read_capped_bytes(file, path, max_bytes)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Reads an open `file` whole when it is a regular file of the euid no longer than `max_bytes`, checked on the handle
/// before reading and enforced while reading, so a file that grows meanwhile is refused too.
fn read_capped_bytes(file: std::fs::File, path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind, Read};
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    let over_cap = |n| {
        Error::new(
            ErrorKind::InvalidData,
            format!("{} is {n} bytes, over the {max_bytes}-byte cap", path.display()),
        )
    };
    if meta.len() > max_bytes {
        return Err(over_cap(meta.len()));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(over_cap(bytes.len() as u64));
    }
    Ok(bytes)
}

/// Why a user's configuration directory is not used. Every later step works from the handle a good
/// directory gives, so what was judged is what is written to.
#[derive(Debug)]
pub enum DirRefusal {
    NotADirectory,
    NotOwned,
    Writable,
    UnsafeParent,
    SymlinkChain,
    MissingParent,
    Io(std::io::Error),
}

impl std::fmt::Display for DirRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirRefusal::NotADirectory => f.write_str("it is not a directory"),
            DirRefusal::NotOwned => f.write_str("it belongs to another user"),
            DirRefusal::Writable => f.write_str("its group or others can write to it"),
            DirRefusal::UnsafeParent => f.write_str("another user could replace it in the directory that holds it"),
            DirRefusal::SymlinkChain => {
                f.write_str("it is a link to a link, and one link is the most that is followed")
            }
            DirRefusal::MissingParent => f.write_str("the directory that should hold it does not exist"),
            DirRefusal::Io(e) => write!(f, "{e}"),
        }
    }
}

/// The rule for the directory itself, on what `fstat` said of the open handle: a directory owned by
/// `euid` that neither its group nor others can write. `mode` is `st_mode`, file type included.
pub(crate) fn config_dir_checks(mode: u32, owner: u32, euid: u32) -> Result<(), DirRefusal> {
    if mode as libc::mode_t & libc::S_IFMT != libc::S_IFDIR {
        return Err(DirRefusal::NotADirectory);
    }
    if owner != euid {
        return Err(DirRefusal::NotOwned);
    }
    if mode & 0o022 != 0 {
        return Err(DirRefusal::Writable);
    }
    Ok(())
}

/// What a name in a directory is, without following a link or opening anything: `open` on a symlink
/// with `O_DIRECTORY|O_NOFOLLOW` says `ENOTDIR`, the same as for a plain file, so only a look at the
/// entry itself tells them apart, and a FIFO is never opened at all.
enum Entry {
    Missing,
    Directory,
    Link,
    Other,
}

fn entry_at(holder: &std::fs::File, name: &CStr) -> Result<Entry, DirRefusal> {
    let Some(stat) = stat_at(holder, name).map_err(DirRefusal::Io)? else {
        return Ok(Entry::Missing);
    };
    Ok(match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => Entry::Directory,
        libc::S_IFLNK => Entry::Link,
        _ => Entry::Other,
    })
}

/// `fstatat` of the entry `name` in `holder` itself, a link included; `None` when nothing is there.
fn stat_at(holder: &std::fs::File, name: &CStr) -> std::io::Result<Option<libc::stat>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is `holder`'s own and open, `name` is NUL-terminated, and `stat` is a
    // buffer of the size `fstatat` writes; all three outlive the call.
    let looked = unsafe {
        libc::fstatat(
            holder.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if looked != 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(e),
        };
    }
    // SAFETY: `fstatat` returned 0, so it filled the buffer.
    Ok(Some(unsafe { stat.assume_init() }))
}

/// Opens the directory that holds the last component of `path`, which `base` resolves when `path` is
/// relative. Links on the way there are followed: `~/.config` may be one. Returns the handle and the
/// last component.
fn open_holder(base: RawFd, path: &Path) -> Result<(std::fs::File, CString), DirRefusal> {
    // `file_name` is `None` for `/` and for a path ending in `..`: no entry of a directory to look at.
    let name = path.file_name().ok_or(DirRefusal::NotADirectory)?;
    let holder = path.parent().filter(|p| !p.as_os_str().is_empty());
    let holder = holder.unwrap_or(Path::new(".")).as_os_str();
    let (Ok(name), Ok(holder)) = (CString::new(name.as_bytes()), CString::new(holder.as_bytes())) else {
        return Err(DirRefusal::Io(std::io::ErrorKind::InvalidInput.into()));
    };
    // SAFETY: `holder` is NUL-terminated and outlives the call; `base` is `AT_FDCWD` or a descriptor the
    // caller holds open for it.
    let fd = unsafe {
        libc::openat(
            base,
            holder.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => DirRefusal::MissingParent,
            _ => DirRefusal::Io(e),
        });
    }
    // SAFETY: `fd` was just returned by `openat` and is owned by nothing else.
    Ok((unsafe { std::fs::File::from_raw_fd(fd) }, name))
}

/// [`parent_is_safe`] on the open handle, which is the directory links led to: its entries are the
/// ones that could be renamed.
fn judge_holder(holder: &std::fs::File) -> Result<(), DirRefusal> {
    let meta = holder.metadata().map_err(DirRefusal::Io)?;
    if parent_is_safe(meta.uid(), meta.mode(), euid()) {
        Ok(())
    } else {
        Err(DirRefusal::UnsafeParent)
    }
}

/// Opens the real directory `name` in `holder` and applies [`config_dir_checks`] to the handle. A
/// failure that says the entry is not a directory means it changed since it was looked at.
fn open_config_dir_at(holder: &std::fs::File, name: &CStr) -> Result<std::fs::File, DirRefusal> {
    let handle = openat_dir(holder, name).map_err(|e| match e.raw_os_error() {
        Some(libc::ENOTDIR | libc::ELOOP) => DirRefusal::NotADirectory,
        _ => DirRefusal::Io(e),
    })?;
    judge_config_dir(&handle)?;
    Ok(handle)
}

fn judge_config_dir(handle: &std::fs::File) -> Result<(), DirRefusal> {
    let meta = handle.metadata().map_err(DirRefusal::Io)?;
    config_dir_checks(meta.mode(), meta.uid(), euid())
}

/// Reads the link `name` in `holder` once.
fn read_link_at(holder: &std::fs::File, name: &CStr) -> std::io::Result<std::path::PathBuf> {
    let mut target = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the descriptor is `holder`'s own and open, `name` is NUL-terminated, and `target` is a
    // buffer of the length given; all outlive the call.
    let read = unsafe {
        libc::readlinkat(
            holder.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    let Ok(read) = usize::try_from(read) else {
        return Err(std::io::Error::last_os_error());
    };
    if read == target.len() {
        return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    target.truncate(read);
    Ok(OsStr::from_bytes(&target).into())
}

/// Follows the link `name` in `holder`, the one link there may be: the target is looked at without
/// following anything, so a target that is itself a link is refused. A relative target resolves
/// against the directory the link sits in. The directory that holds the target is the one judged:
/// swapping the link itself can only lead to another directory of this user's, as the target is
/// judged for its owner.
fn open_link_target(holder: &std::fs::File, name: &CStr) -> Result<std::fs::File, DirRefusal> {
    let target = read_link_at(holder, name).map_err(DirRefusal::Io)?;
    let (target_holder, target_name) = open_holder(holder.as_raw_fd(), &target)?;
    judge_holder(&target_holder)?;
    match entry_at(&target_holder, &target_name)? {
        Entry::Directory => open_config_dir_at(&target_holder, &target_name),
        Entry::Link => Err(DirRefusal::SymlinkChain),
        Entry::Missing | Entry::Other => Err(DirRefusal::NotADirectory),
    }
}

/// The directory `name` in `holder`, judged; `None` when there is nothing at that name.
fn find_config_dir(holder: &std::fs::File, name: &CStr) -> Result<Option<std::fs::File>, DirRefusal> {
    match entry_at(holder, name)? {
        Entry::Missing => judge_holder(holder).map(|()| None),
        Entry::Directory => {
            judge_holder(holder)?;
            open_config_dir_at(holder, name).map(Some)
        }
        Entry::Link => open_link_target(holder, name).map(Some),
        Entry::Other => Err(DirRefusal::NotADirectory),
    }
}

/// Opens the user's configuration directory `path` when it is one only this user can change: a real
/// directory, or one link to one, owned by the euid and not writable by its group or others, held by a
/// directory [`parent_is_safe`] accepts (for a link, the target's). `Ok(None)` is nothing at `path` and a
/// safe directory to make it in. Creates nothing and changes nothing.
pub fn probe_user_config_dir(path: &Path) -> Result<Option<OwnedFd>, DirRefusal> {
    let (holder, name) = open_holder(libc::AT_FDCWD, path)?;
    Ok(find_config_dir(&holder, &name)?.map(OwnedFd::from))
}

/// [`probe_user_config_dir`], making the last component of `path` when it is missing: exactly 0700
/// whatever the umask, from the handle of the directory just judged. One that appears meanwhile (a
/// second window) is judged like any other.
pub fn open_user_config_dir(path: &Path) -> Result<OwnedFd, DirRefusal> {
    let (holder, name) = open_holder(libc::AT_FDCWD, path)?;
    if let Some(found) = find_config_dir(&holder, &name)? {
        return Ok(found.into());
    }
    let made = make_dir_at(&holder, OsStr::from_bytes(name.as_bytes()), path).map_err(DirRefusal::Io)?;
    judge_config_dir(&made)?;
    Ok(made.into())
}

/// Why [`create_new_file_at`] put no file there.
#[derive(Debug)]
pub enum CreateRefusal {
    /// Something is at the name, whatever it is; it was not opened or changed.
    Exists,
    /// This file system or kernel has neither a hard link nor a rename that never replaces, so a new
    /// name cannot be made to appear whole.
    CannotCreateSafely,
    Io(std::io::Error),
}

/// The two calls that give a finished temporary file its name, apart so a test can make them fail as a
/// file system would. Both are relative to `dir` and neither replaces what is at `to`.
pub(crate) trait LinkCalls {
    fn link(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()>;
    fn rename_noreplace(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()>;
}

struct SystemCalls;

impl LinkCalls for SystemCalls {
    fn link(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()> {
        let fd = dir.as_raw_fd();
        // SAFETY: `dir` is open for the call and both names are NUL-terminated and outlive it.
        if unsafe { libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "linux")]
    fn rename_noreplace(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()> {
        let fd = dir.as_raw_fd();
        // SAFETY: `dir` is open for the call and both names are NUL-terminated and outlive it.
        if unsafe { libc::renameat2(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_NOREPLACE) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Only Linux has this rename here, so elsewhere a file system without hard links gets no file.
    #[cfg(not(target_os = "linux"))]
    fn rename_noreplace(&self, _dir: BorrowedFd<'_>, _from: &CStr, _to: &CStr) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

/// Numbers this process's temporary files, so two threads creating one at once never share a name.
static CREATE_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Names tried for a temporary file before giving up: one that is taken was left by a process that
/// ended with this one's id, and the next serial is another name.
const TEMPORARY_TRIES: u32 = 8;

/// A new file in `dir` under `<stem>.<pid>.<serial>.tmp`, never one that was there.
fn create_temporary(dir: BorrowedFd<'_>, stem: &OsStr, mode: u32) -> std::io::Result<(CString, std::fs::File)> {
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_WRONLY;
    for _ in 0..TEMPORARY_TRIES {
        let serial = CREATE_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp = stem.to_os_string();
        tmp.push(format!(".{}.{serial}.tmp", std::process::id()));
        let tmp = CString::new(tmp.as_bytes())?;
        // SAFETY: `dir` is open for the call and `tmp` is NUL-terminated and outlives it; the mode goes
        // through the variadic as the `c_uint` a `mode_t` is promoted to.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp.as_ptr(), flags, mode as libc::c_uint) };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by `openat` and is owned by nothing else.
            return Ok((tmp, unsafe { std::fs::File::from_raw_fd(fd) }));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e);
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// Gives the finished temporary file its name, by the first of the two calls this file system has.
fn place(calls: &dyn LinkCalls, dir: BorrowedFd<'_>, tmp: &CStr, name: &CStr) -> Result<(), CreateRefusal> {
    let Err(linked) = calls.link(dir, tmp, name) else {
        return Ok(());
    };
    match linked.raw_os_error() {
        Some(libc::EEXIST) => return Err(CreateRefusal::Exists),
        Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS) => {}
        _ => return Err(CreateRefusal::Io(linked)),
    }
    calls
        .rename_noreplace(dir, tmp, name)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::EEXIST) => CreateRefusal::Exists,
            Some(libc::EINVAL | libc::ENOSYS) => CreateRefusal::CannotCreateSafely,
            _ => CreateRefusal::Io(e),
        })
}

/// Creates the file `name` in the directory `dir` holds open, with `bytes` as its content, or leaves
/// the directory as it was. The file appears whole: it is written and synced under a temporary name,
/// and only then given `name` by a call that fails when anything is there, a link or a FIFO included,
/// which is never opened. Nothing here can replace or truncate a file, and there is no fallback that
/// creates the file first and writes it after: a crash there would leave part of a file at `name`.
///
/// `mode` is the mode asked of `open`, so the umask has its say.
pub fn create_new_file_at(dir: BorrowedFd<'_>, name: &str, bytes: &[u8], mode: u32) -> Result<(), CreateRefusal> {
    create_new_file_with(&SystemCalls, dir, name, bytes, mode)
}

/// [`create_new_file_at`] over `calls`.
pub(crate) fn create_new_file_with(
    calls: &dyn LinkCalls,
    dir: BorrowedFd<'_>,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), CreateRefusal> {
    let c_name = entry_name(OsStr::new(name)).map_err(CreateRefusal::Io)?;
    let (tmp, mut file) = create_temporary(dir, OsStr::new(name), mode).map_err(CreateRefusal::Io)?;
    let placed = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(CreateRefusal::Io)
        .and_then(|()| place(calls, dir, &tmp, &c_name));
    drop(file);
    // After a rename the temporary name is already gone, and a leftover harms nothing, so the answer
    // does not depend on this.
    // SAFETY: `dir` is open for the call and `tmp` is NUL-terminated and outlives it.
    unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
    if placed.is_ok() {
        // The file is there by now, so a failure here must not read as "nothing was written"; it only
        // means a crash could still lose the new name.
        // SAFETY: `dir` is open for the call; `fsync` takes no pointers.
        unsafe { libc::fsync(dir.as_raw_fd()) };
    }
    placed
}

/// `name` as one entry of a directory, or `InvalidInput`: `.`, `..` and a path lead out of the entry
/// the caller meant, and a NUL cannot reach the system at all.
fn entry_name(name: &OsStr) -> std::io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(CString::new(bytes)?)
}

/// `name` in `parent` as an open directory handle. A missing one is made with `mkdirat` and set to exactly
/// 0700 whatever the umask. An existing one must be a real directory (a link is not followed) of the euid
/// that neither its group nor others can write, sticky or not, and its mode is left as it is, however
/// narrow or wide within that. Anything else at the name is `PermissionDenied`, and a FIFO is never opened
/// as one. Never changes a directory it did not make, so it fits a directory another program keeps, where
/// [`create_private_dir_all`] does not. Another process of this user can swap the new directory for its own
/// before the handle opens: one with a bit for group or others is told apart and kept as it is, while one
/// narrower than 0700 reads as the umask's work and is set. A `name` that is not one entry of a directory
/// is `InvalidInput`.
pub fn ensure_child_dir(parent: &std::fs::File, name: &OsStr) -> std::io::Result<std::fs::File> {
    let refused = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a real directory of this user that only this user can write to",
                Path::new(name).display()
            ),
        )
    };
    let cname = entry_name(name)?;
    let made = mkdir_at(parent, &cname)?;
    #[cfg(test)]
    run_after_mkdir_hook(Path::new(name));
    // `open` with `O_DIRECTORY|O_NOFOLLOW` says `ENOTDIR` for a link on Linux and `ELOOP` elsewhere.
    let handle = open_after_mkdir(parent, &cname, made).map_err(|e| match e.raw_os_error() {
        Some(libc::ENOTDIR | libc::ELOOP) => refused(),
        _ => e,
    })?;
    let meta = handle.metadata()?;
    config_dir_checks(meta.mode(), meta.uid(), euid()).map_err(|_| refused())?;
    if made && could_be_from_mkdir(meta.mode()) {
        fchmod(&handle, PRIVATE_DIR_MODE)?;
    }
    Ok(handle)
}

/// `path` opened as a directory (`O_DIRECTORY | O_CLOEXEC`), with no test of its owner or mode: for a
/// home, which is shared with every program and may be a link. What is opened is only a place to start
/// from; [`ensure_child_dir`] judges what is inside it.
pub fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

/// Which file a name holds: the device and inode, which a rename over the name changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Publishes `bytes` as `name` in `dir`, replacing whatever file or link is there without opening it. The
/// file is made in the same directory as `.<name>.<pid>.<serial>.tmp`, so a reader that looks for names
/// ending as `name` does never meets it half-written, with `O_CREAT | O_EXCL | O_NOFOLLOW | O_NONBLOCK |
/// O_CLOEXEC` through `openat`. It is set to exactly 0600 on the handle whatever the umask, written whole
/// and `fsync`ed, and only then renamed over `name` within the directory's handle. On any failure the
/// temporary name is removed. Returns the identity of the file that took the name, for [`remove_if_same`].
pub fn publish_private_file(dir: &std::fs::File, name: &OsStr, bytes: &[u8]) -> std::io::Result<FileIdentity> {
    let target = entry_name(name)?;
    let mut stem = OsString::from(".");
    stem.push(name);
    let (tmp, mut file) = create_temporary(dir.as_fd(), &stem, PRIVATE_FILE_MODE)?;
    let published = fill_and_rename(&mut file, dir, &tmp, &target, bytes);
    drop(file);
    if published.is_err() {
        // SAFETY: `dir` is open for the call and `tmp` is NUL-terminated and outlives it.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
    }
    published
}

/// The identity comes from the handle before the rename, so nothing that can fail runs once the file has
/// taken the name: an error always means `name` was left as it was.
fn fill_and_rename(
    file: &mut std::fs::File,
    dir: &std::fs::File,
    tmp: &CStr,
    name: &CStr,
    bytes: &[u8],
) -> std::io::Result<FileIdentity> {
    fchmod(file, PRIVATE_FILE_MODE)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let meta = file.metadata()?;
    #[cfg(test)]
    publish_test_hooks::before_rename();
    let fd = dir.as_raw_fd();
    // SAFETY: `dir` is open for the call and both names are NUL-terminated and outlive it.
    if unsafe { libc::renameat(fd, tmp.as_ptr(), fd, name.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Removes `name` from `dir` only when it is still the file `identity` names, looked at with `fstatat`
/// and `AT_SYMLINK_NOFOLLOW`: `true` when this call removed it, `false` when something else is there or
/// nothing is. The look and the `unlinkat` are two steps, so a rename between them is not seen; only a
/// process of this user can do that in a directory [`ensure_child_dir`] accepted.
pub fn remove_if_same(dir: &std::fs::File, name: &OsStr, identity: FileIdentity) -> std::io::Result<bool> {
    let cname = entry_name(name)?;
    let Some(stat) = stat_at(dir, &cname)? else {
        return Ok(false);
    };
    // The widths of these two fields differ from one unix to another.
    #[allow(clippy::unnecessary_cast)]
    let named = FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    };
    if named != identity {
        return Ok(false);
    }
    // SAFETY: `dir` is open for the call and `cname` is NUL-terminated and outlives it.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), cname.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT) => Ok(false),
        _ => Err(e),
    }
}

/// A hook a test sets on its own thread, called in [`publish_private_file`] once the temporary file is
/// written and before it takes its name, so the directory can be looked at in that moment.
#[cfg(test)]
mod publish_test_hooks {
    use std::cell::RefCell;

    thread_local! {
        static BEFORE_RENAME: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn set_before_rename(hook: impl FnOnce() + 'static) {
        BEFORE_RENAME.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn before_rename() {
        let hook = BEFORE_RENAME.with(|h| h.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_capped_read_refuses_a_file_over_the_cap_before_reading_it() {
        let dir = scratch("capped-large");
        let path = dir.join("theme.json");
        std::fs::File::create(&path).unwrap().set_len(257).unwrap();
        let err = read_private_to_string_capped(&path, 256).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("257 bytes, over the 256-byte cap"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_capped_read_reads_a_file_exactly_at_the_cap() {
        let dir = scratch("capped-exact");
        let path = dir.join("theme.json");
        let text = "é".repeat(128);
        write_private(&path, text.as_bytes()).unwrap();
        assert_eq!(read_private_to_string_capped(&path, 256).unwrap(), text);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_capped_read_refuses_a_fifo() {
        let dir = scratch("capped-fifo");
        let path = dir.join("theme.json");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path remains valid for the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            read_private_to_string_capped(&path, 256).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A name another process may have put in place is read only when it is a regular file of this user: a link is
    /// refused rather than followed, and a FIFO is refused at once rather than waited on.
    #[test]
    fn nofollow_read_refuses_a_symlink_and_a_fifo_and_reads_a_file() {
        let dir = scratch("nofollow");
        let file = dir.join("out.json");
        write_private(&file, b"{\"same\":true}").unwrap();
        assert_eq!(read_private_nofollow_capped(&file, 256).unwrap(), "{\"same\":true}");

        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(
            read_private_nofollow_capped(&link, 256).is_err(),
            "a link is not followed"
        );

        let fifo = dir.join("fifo.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path remains valid for the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_private_nofollow_capped(&path, 256).map_err(|e| e.kind()));
        });
        let read = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read blocked on a FIFO");
        assert_eq!(read, Err(std::io::ErrorKind::PermissionDenied));

        assert_eq!(
            read_private_nofollow_capped(&file, 4).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "over the cap"
        );
        assert_eq!(
            read_private_nofollow_capped(&dir.join("missing"), 256)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The raw form refuses what the text form refuses: a link is not followed, a FIFO does not hold it, and the cap
    /// holds.
    #[test]
    fn bytes_reader_refuses_a_link_a_fifo_and_past_the_cap() {
        let dir = scratch("bytes-refusals");
        let file = dir.join("lock");
        write_private(&file, b"/home/someone/project").unwrap();
        assert_eq!(
            read_private_nofollow_bytes_capped(&file, 256).unwrap(),
            b"/home/someone/project"
        );

        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(
            read_private_nofollow_bytes_capped(&link, 256).is_err(),
            "a link is not followed"
        );

        let fifo = dir.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path remains valid for the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_private_nofollow_bytes_capped(&path, 256).map_err(|e| e.kind()));
        });
        let read = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read blocked on a FIFO");
        assert_eq!(read, Err(std::io::ErrorKind::PermissionDenied));

        assert_eq!(
            read_private_nofollow_bytes_capped(&file, 4).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "over the cap"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bytes_reader_refuses_a_file_of_another_uid() {
        assert_eq!(
            read_private_nofollow_bytes_capped(Path::new("/etc/passwd"), 1 << 20)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn bytes_reader_reads_non_utf8() {
        let dir = scratch("bytes-raw");
        let file = dir.join("lock");
        write_private(&file, b"/home/\xff/project").unwrap();
        assert_eq!(
            read_private_nofollow_bytes_capped(&file, 256).unwrap(),
            b"/home/\xff/project"
        );
        assert_eq!(
            read_private_nofollow_capped(&file, 256).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "the text form still refuses it"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_capped_read_of_a_missing_file_is_not_found() {
        let dir = scratch("capped-missing");
        assert_eq!(
            read_private_to_string_capped(&dir.join("missing"), 256)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        path.symlink_metadata().unwrap().mode() & 0o777
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nv-private-fs-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Umask 002 otherwise permits group writes (`0775`/`0664`); the modes here are set
    /// explicitly, so the process umask cannot widen them. The umask is process-wide, so the body
    /// runs in a child copy of the test binary (`test_umask`).
    #[test]
    fn new_directories_are_0700_and_files_0600_whatever_the_umask() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::new_directories_are_0700_and_files_0600_whatever_the_umask"
            ),
            0o002,
            || {
                let base = scratch("umask");
                let root = base.join("eitri");
                let dir = root.join("conversations").join("abc");
                let file = dir.join("record.json");
                create_private_dir_all(&dir, &root)
                    .and_then(|()| write_private(&file, b"{}"))
                    .unwrap();
                assert_eq!(mode(&root), 0o700);
                assert_eq!(mode(&root.join("conversations")), 0o700);
                assert_eq!(mode(&dir), 0o700);
                assert_eq!(mode(&file), 0o600);
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// A umask that takes the owner's own bits away (0277 leaves `0500` directories and `0400`
    /// files) must not leave a directory nothing can be created in or a file nothing can rewrite:
    /// the modes are set exactly, not only tightened.
    #[test]
    fn a_umask_that_takes_the_owners_bits_still_gives_0700_and_0600() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::a_umask_that_takes_the_owners_bits_still_gives_0700_and_0600"
            ),
            0o277,
            || {
                let base = scratch("umask-owner");
                // Above the root nothing is ever changed, so the test opens up its own scratch,
                // which the umask made 0500 too, as a user's state home would already be.
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
                let root = base.join("eitri");
                let dir = root.join("conversations").join("abc");
                let file = dir.join("record.json");
                create_private_dir_all(&dir, &root)
                    .and_then(|()| write_private(&file, b"{}"))
                    .unwrap();
                assert_eq!(mode(&root), 0o700);
                assert_eq!(mode(&root.join("conversations")), 0o700);
                assert_eq!(mode(&dir), 0o700);
                assert_eq!(mode(&file), 0o600);
                write_private(&file, b"{\"again\":1}").unwrap();
                assert_eq!(std::fs::read(&file).unwrap(), b"{\"again\":1}");
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// A directory just made is set to 0700 through the handle it was made by, not by looking its path
    /// up again: another user who can write the directory above `eitri` could swap `eitri` for a link
    /// between the two steps, and the second step would then change the mode of whatever the link
    /// leads to (here the victim's own `conversations`, 0755).
    #[test]
    fn a_directory_swapped_for_a_link_right_after_it_was_made_is_not_followed() {
        let base = scratch("swap");
        let victim = base.join("victim");
        std::fs::create_dir_all(victim.join("conversations")).unwrap();
        std::fs::set_permissions(victim.join("conversations"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = base.join("eitri");
        let dir = root.join("conversations");
        let swap = {
            let (base, victim, root, dir) = (base.clone(), victim.clone(), root.clone(), dir.clone());
            move |made: &Path| {
                if made == dir {
                    std::fs::rename(&root, base.join("eitri.moved")).unwrap();
                    std::os::unix::fs::symlink(&victim, &root).unwrap();
                }
            }
        };
        AFTER_MKDIR.with(|hook| *hook.borrow_mut() = Some(Box::new(swap)));
        let _ = create_private_dir_all(&dir, &root);
        AFTER_MKDIR.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(
            mode(&victim.join("conversations")),
            0o755,
            "a directory the link led to was changed"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// An older build's `0755` tree is tightened on the next write, from the root down; nothing
    /// above the root is touched.
    #[test]
    fn an_older_builds_open_directories_are_tightened_from_the_root_down() {
        let base = scratch("tighten");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = base.join("eitri");
        let dir = root.join("history");
        std::fs::create_dir_all(&dir).unwrap();
        for d in [&root, &dir] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_file = dir.join("old.jsonl");
        std::fs::write(&old_file, "x").unwrap();
        std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o644)).unwrap();

        create_private_dir_all(&dir, &root).unwrap();
        write_private(&old_file, b"y").unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&old_file), 0o600, "an existing file is narrowed on its next write");
        assert_eq!(mode(&base), 0o755, "above the root is never changed");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A root a restrictive umask left at 0500 by an older build, with nothing inside it yet, is set
    /// to 0700 before anything is made in it: otherwise the first directory below it cannot be
    /// created and the root is never repaired.
    #[test]
    fn a_0500_root_is_repaired_before_a_directory_is_made_in_it() {
        let base = scratch("root-0500");
        let root = base.join("eitri");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let dir = root.join("conversations").join("abc");
        let made = create_private_dir_all(&dir, &root);
        // Open it up again before asserting, so a failure still leaves a removable scratch.
        let root_mode = mode(&root);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        made.unwrap();
        assert_eq!(root_mode, 0o700);
        assert_eq!(mode(&root.join("conversations")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The same for a directory between the root and the one asked for.
    #[test]
    fn a_0500_directory_below_the_root_is_repaired_before_a_directory_is_made_in_it() {
        let base = scratch("middle-0500");
        let root = base.join("eitri");
        let middle = root.join("conversations");
        std::fs::create_dir_all(&middle).unwrap();
        std::fs::set_permissions(&middle, std::fs::Permissions::from_mode(0o500)).unwrap();
        let dir = middle.join("abc");
        let made = create_private_dir_all(&dir, &root);
        let middle_mode = mode(&middle);
        std::fs::set_permissions(&middle, std::fs::Permissions::from_mode(0o700)).unwrap();
        made.unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(middle_mode, 0o700);
        assert_eq!(mode(&dir), 0o700);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A symlink in the root's place is neither followed nor chmodded.
    #[test]
    fn a_symlinked_directory_is_not_tightened() {
        let base = scratch("symlink");
        let target = base.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = base.join("eitri");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        tighten_own_dir(&link);
        assert_eq!(mode(&target), 0o755);
        assert!(!is_own_real_dir(&link));
        assert!(is_own_real_dir(&target));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// C3: a symlinked `root` must skip tightening altogether, never walk through it by path.
    /// `root` (`eitri`) is a symlink to `target`; `target/sub` pre-exists at 0755.
    /// `create_private_dir_all(root/sub/x, root)` must never touch `target/sub`'s mode -- before
    /// the fix, `tighten_own_dir(root)` itself failed closed (`ELOOP`) but the loop's next path,
    /// `root/sub`, resolved straight through the symlink and fchmodded `target/sub` to 0700.
    #[test]
    fn a_symlinked_root_is_never_walked_through() {
        let base = scratch("symlinked-root");
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        let sub = target.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = base.join("eitri");
        std::os::unix::fs::symlink(&target, &root).unwrap();

        let dir = root.join("sub").join("x");
        create_private_dir_all(&dir, &root).unwrap();

        assert_eq!(
            mode(&sub),
            0o755,
            "a symlinked root must never be walked through to reach and tighten what it points at"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// C3: a symlinked middle component (`root/conversations` -> `other/`) must stop the walk at
    /// that component, never resolve through it to tighten `other/abc`. The control shows `root`
    /// itself IS still tightened -- only the walk past the symlink is refused.
    #[test]
    fn a_symlinked_middle_component_is_never_walked_through() {
        let base = scratch("symlinked-middle");
        let root = base.join("eitri");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let other = base.join("other");
        std::fs::create_dir(&other).unwrap();
        let abc = other.join("abc");
        std::fs::create_dir(&abc).unwrap();
        std::fs::set_permissions(&abc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let conversations = root.join("conversations");
        std::os::unix::fs::symlink(&other, &conversations).unwrap();

        let dir = root.join("conversations").join("abc");
        create_private_dir_all(&dir, &root).unwrap();

        assert_eq!(
            mode(&abc),
            0o755,
            "a symlinked middle component must never be walked through to tighten what it points at"
        );
        assert_eq!(mode(&root), 0o700, "the control: root itself is still tightened");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A FIFO planted where a log file goes must be refused, not waited on: a blocking `open` for
    /// writing never returns while nothing reads the other end.
    #[test]
    fn a_fifo_in_place_of_the_file_is_refused_without_blocking() {
        let base = scratch("fifo");
        let fifo = base.join("x.log");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_private(&path).is_err());
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("open_private blocked on a FIFO");
        assert!(refused, "a FIFO is not a regular file");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Two opens of one lock file exclude each other, in one process too: the second try fails while the
    /// first holds it and succeeds once that one is dropped. The file is 0600 and never truncated.
    ///
    /// "Once dropped" is not "at the same instant": the lib tests run on many threads and some spawn a
    /// process, and a child forked while the first lock is held shares its open file description until
    /// the child's `exec` closes it (`O_CLOEXEC`), so the lock can stay held for a moment after the drop.
    /// The test therefore tries again until the lock is free, within a hang guard that only stops a lock
    /// that never frees; it asserts no wall-clock bound on how soon.
    #[test]
    fn a_private_lock_excludes_another_open_of_the_same_file_until_dropped() {
        let base = scratch("lock");
        let path = base.join("store.json.lock");
        std::fs::write(&path, b"kept").unwrap();
        let first = PrivateLock::open(&path).unwrap();
        assert!(first.try_lock().unwrap());
        assert!(first.try_lock().unwrap(), "the holder trying again still holds it");
        let second = PrivateLock::open(&path).unwrap();
        assert!(!second.try_lock().unwrap(), "held by the first open");
        drop(first);
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut free = second.try_lock().unwrap();
        while !free && std::time::Instant::now() < give_up {
            std::thread::sleep(std::time::Duration::from_millis(2));
            free = second.try_lock().unwrap();
        }
        assert!(free, "free once the first is dropped");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"kept");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A forked child shares every open file description it is given until its `exec`, and a lock lives as
    /// long as its description does, so the lock file's descriptor must close on exec -- whether the file
    /// was there or is made by this open -- or a spawned program would hold a store locked for as long
    /// as it ran.
    #[test]
    fn a_private_lock_descriptor_closes_on_exec() {
        use std::os::fd::AsRawFd;
        let base = scratch("lock-cloexec");
        let path = base.join("store.json.lock");
        for round in ["made by the open", "already there"] {
            let lock = PrivateLock::open(&path).unwrap();
            // SAFETY: the descriptor is the lock's own and open; `F_GETFD` takes no pointers.
            let flags = unsafe { libc::fcntl(lock.file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0, "{round}: F_GETFD failed");
            assert!(
                flags & libc::FD_CLOEXEC != 0,
                "{round}: the lock descriptor is inherited across exec"
            );
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Under a umask that takes the owner's own bits away (0277) a file is created 0400, and stays so until
    /// its mode is set. A second window opening the lock file at that moment must get it all the same, and
    /// the two opens must be of the one file, or a window's write would be refused (or not excluded) only
    /// because another was creating the lock. The umask is process-wide, so the body runs in a child copy
    /// of the test binary.
    #[test]
    fn a_lock_file_being_created_under_a_narrow_umask_never_refuses_another_window() {
        if current_uid() == 0 {
            return;
        }
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::a_lock_file_being_created_under_a_narrow_umask_never_refuses_another_window"
            ),
            0o277,
            || {
                let base = scratch("lock-umask");
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
                let path = base.join("store.json.lock");
                let (created_tx, created) = std::sync::mpsc::channel();
                let (go_tx, go) = std::sync::mpsc::channel::<()>();
                let a = {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        lock_test_hooks::set_after_create(move || {
                            created_tx.send(()).unwrap();
                            go.recv().unwrap();
                        });
                        PrivateLock::open(&path)
                    })
                };
                created.recv().unwrap();
                let b = PrivateLock::open(&path);
                go_tx.send(()).unwrap();
                let a = a.join().unwrap().expect("window A's open");
                let b = b.expect("window B's open, while window A was creating the lock file");
                assert!(a.try_lock().unwrap());
                assert!(!b.try_lock().unwrap(), "both opened the one lock file");
                drop(a);
                assert!(b.try_lock().unwrap());
                assert_eq!(mode(&path), 0o600);
                drop(b);
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// A directory where the lock file goes is refused, not locked or replaced, and so is any later open
    /// while it stays. A lock file made in an empty place leaves no temporary name behind.
    #[test]
    fn a_private_lock_refuses_a_directory_and_leaves_no_temporary_file() {
        let base = scratch("lock-dir");
        let dir = base.join("store.json.lock");
        std::fs::create_dir(&dir).unwrap();
        assert!(PrivateLock::open(&dir).is_err());
        assert!(PrivateLock::open(&dir).is_err(), "still refused, never replaced");
        assert!(dir.is_dir());
        let path = base.join("other.json.lock");
        let lock = PrivateLock::open(&path).unwrap();
        assert!(lock.try_lock().unwrap());
        let mut names: Vec<String> = std::fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["other.json.lock", "store.json.lock"]);
        drop(lock);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A lock file is opened as a private file is: a symlink or a FIFO in its place is refused.
    #[test]
    fn a_private_lock_refuses_a_symlink_and_a_fifo() {
        let base = scratch("lock-planted");
        let link = base.join("link.lock");
        std::os::unix::fs::symlink(base.join("elsewhere"), &link).unwrap();
        assert!(PrivateLock::open(&link).is_err());
        assert!(!base.join("elsewhere").exists(), "nothing created through the link");
        let fifo = base.join("fifo.lock");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // `O_NONBLOCK`: a FIFO with no writer opens at once, so this returns and is then refused by type.
        assert!(PrivateLock::open(&fifo).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// An existing file that is not ours is refused before anything is truncated or changed.
    #[test]
    fn a_file_owned_by_someone_else_is_refused_untouched() {
        if current_uid() == 0 {
            return;
        }
        // `/etc/hostname` is not guaranteed; any root-owned, world-readable file will do. macOS's
        // `/etc/passwd` is stored compressed, and opening such a file for writing blocks in the kernel
        // while it is decompressed, so there the target is a file that is not.
        #[cfg(target_os = "macos")]
        let theirs = Path::new("/etc/hosts");
        #[cfg(not(target_os = "macos"))]
        let theirs = Path::new("/etc/passwd");
        assert!(open_private(theirs).is_err());
        assert!(std::fs::metadata(theirs).unwrap().len() > 0);
    }

    #[test]
    fn a_new_file_by_name_is_private_and_never_opens_what_exists() {
        let dir = scratch("create-at");
        let handle = open_dir(&dir).unwrap();
        let mut file = create_private_file_at(handle.as_fd(), c"record").unwrap();
        file.write_all(b"written").unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("record")).unwrap().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        let again = create_private_file_at(handle.as_fd(), c"record").unwrap_err();
        assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(dir.join("record")).unwrap(), b"written");
        std::os::unix::fs::symlink(dir.join("record"), dir.join("link")).unwrap();
        assert!(create_private_file_at(handle.as_fd(), c"link").is_err());
        assert_eq!(std::fs::read(dir.join("record")).unwrap(), b"written");
    }

    /// A directory that belongs to another user is not ours to put records in, nor to tighten.
    #[test]
    fn a_directory_owned_by_someone_else_is_refused() {
        if current_uid() == 0 {
            return;
        }
        let theirs = Path::new("/usr/share");
        let err = create_private_dir_all(theirs, theirs).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        let err = create_private_dir_all(theirs, Path::new("/usr")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    }

    /// `root` itself is judged too: a `<state home>/eitri` that another user created first, open to
    /// everyone, would let them swap a directory of ours inside it for one of their own.
    #[test]
    fn a_root_owned_by_someone_else_is_refused_even_below_it() {
        let shared = Path::new("/tmp");
        if current_uid() == 0
            || std::fs::metadata(shared)
                .map(|m| m.uid() == current_uid())
                .unwrap_or(true)
        {
            return;
        }
        let ours = shared.join(format!("nv-private-fs-root-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&ours).unwrap();
        let err = create_private_dir_all(&ours.join("permissions"), shared).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            !ours.join("permissions").exists(),
            "nothing is created below a foreign root"
        );
        std::fs::remove_dir(&ours).unwrap();
    }

    /// What Eitri reads back must be a regular file of this user: a FIFO would block the read for
    /// good, and another user's file is not state this user wrote.
    #[test]
    fn reading_refuses_a_fifo_without_blocking_and_a_file_of_someone_else() {
        let base = scratch("read");
        let fifo = base.join("rules.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_private_to_string(&path).map_err(|e| e.kind()));
        });
        let read = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read_private_to_string blocked on a FIFO");
        assert_eq!(read, Err(std::io::ErrorKind::PermissionDenied));
        std::fs::remove_file(&fifo).unwrap();

        let ours = base.join("ours.json");
        write_private(&ours, b"{}").unwrap();
        assert_eq!(read_private_to_string(&ours).unwrap(), "{}");
        assert_eq!(
            read_private_to_string(&base.join("missing.json")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        if current_uid() != 0 {
            let theirs = base.join("theirs.json");
            std::os::unix::fs::symlink("/etc/passwd", &theirs).unwrap();
            assert_eq!(
                read_private_to_string(&theirs).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A scratch directory a config directory may sit in, whatever the umask made it.
    fn config_scratch(label: &str) -> std::path::PathBuf {
        let base = scratch(label);
        set_mode(&base, 0o700);
        base
    }

    fn real_config_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        set_mode(path, 0o700);
    }

    fn inode_of(handle: OwnedFd) -> u64 {
        std::fs::File::from(handle).metadata().unwrap().ino()
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn make_fifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn a_real_private_directory_opens() {
        let base = config_scratch("cfg-real");
        let dir = base.join("eitri");
        real_config_dir(&dir);
        let opened = open_user_config_dir(&dir).unwrap();
        assert_eq!(inode_of(opened), dir.metadata().unwrap().ino());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Stow and chezmoi leave the directory as a link into the dotfiles tree, by a relative path or an
    /// absolute one; a relative one is read against the directory the link sits in.
    #[test]
    fn one_symlink_to_a_good_directory_opens() {
        let base = config_scratch("cfg-link");
        let target = base.join("dotfiles").join("eitri");
        real_config_dir(&base.join("dotfiles"));
        real_config_dir(&target);
        let config = base.join("config");
        real_config_dir(&config);
        let relative = config.join("eitri");
        std::os::unix::fs::symlink("../dotfiles/eitri", &relative).unwrap();
        let absolute = config.join("absolute");
        std::os::unix::fs::symlink(&target, &absolute).unwrap();
        for link in [&relative, &absolute] {
            let opened = open_user_config_dir(link).unwrap();
            assert_eq!(inode_of(opened), target.metadata().unwrap().ino(), "{}", link.display());
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn two_symlinks_are_refused() {
        let base = config_scratch("cfg-two-links");
        real_config_dir(&base.join("real"));
        std::os::unix::fs::symlink(base.join("real"), base.join("second")).unwrap();
        let first = base.join("eitri");
        std::os::unix::fs::symlink(base.join("second"), &first).unwrap();
        assert!(matches!(open_user_config_dir(&first), Err(DirRefusal::SymlinkChain)));
        assert!(matches!(probe_user_config_dir(&first), Err(DirRefusal::SymlinkChain)));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Neither a link to nothing nor one into a directory that does not exist is made real.
    #[test]
    fn a_dangling_symlink_is_refused() {
        let base = config_scratch("cfg-dangling");
        let link = base.join("eitri");
        std::os::unix::fs::symlink(base.join("nowhere"), &link).unwrap();
        assert!(open_user_config_dir(&link).is_err());
        assert!(probe_user_config_dir(&link).is_err());
        assert!(!base.join("nowhere").exists());
        let deeper = base.join("deeper");
        std::os::unix::fs::symlink(base.join("no").join("where"), &deeper).unwrap();
        assert!(open_user_config_dir(&deeper).is_err());
        assert!(!base.join("no").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_group_writable_directory_is_refused() {
        let base = config_scratch("cfg-group");
        let dir = base.join("eitri");
        real_config_dir(&dir);
        set_mode(&dir, 0o775);
        assert!(matches!(open_user_config_dir(&dir), Err(DirRefusal::Writable)));
        assert!(matches!(probe_user_config_dir(&dir), Err(DirRefusal::Writable)));
        assert_eq!(mode(&dir), 0o775, "a user's directory is judged, never tightened");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_directory_of_another_uid_is_refused() {
        // `st_mode` values: the file type bits are the same on every Unix.
        const A_DIRECTORY: u32 = 0o040_700;
        const A_FILE: u32 = 0o100_600;
        assert!(config_dir_checks(A_DIRECTORY, 1000, 1000).is_ok());
        assert!(matches!(
            config_dir_checks(A_DIRECTORY, 1234, 1000),
            Err(DirRefusal::NotOwned)
        ));
        assert!(matches!(
            config_dir_checks(A_FILE, 1000, 1000),
            Err(DirRefusal::NotADirectory)
        ));
        if euid() != 0 {
            assert!(matches!(
                open_user_config_dir(Path::new("/usr")),
                Err(DirRefusal::NotOwned)
            ));
        }
    }

    /// Whoever can rename entries in the parent can swap the directory, present or about to be made.
    /// A sticky parent stops them, so it is accepted.
    #[test]
    fn an_unsafe_parent_is_refused() {
        let base = config_scratch("cfg-parent");
        let shared = base.join("shared");
        real_config_dir(&shared);
        let dir = shared.join("eitri");
        real_config_dir(&dir);
        set_mode(&shared, 0o777);
        assert!(matches!(open_user_config_dir(&dir), Err(DirRefusal::UnsafeParent)));
        let missing = shared.join("missing");
        assert!(matches!(open_user_config_dir(&missing), Err(DirRefusal::UnsafeParent)));
        assert!(matches!(probe_user_config_dir(&missing), Err(DirRefusal::UnsafeParent)));
        assert!(!missing.exists());
        set_mode(&shared, 0o1777);
        assert!(open_user_config_dir(&dir).is_ok());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_link_target_with_an_unsafe_parent_is_refused() {
        let base = config_scratch("cfg-link-parent");
        let shared = base.join("shared");
        real_config_dir(&shared);
        real_config_dir(&shared.join("eitri"));
        set_mode(&shared, 0o777);
        let link = base.join("eitri");
        std::os::unix::fs::symlink(shared.join("eitri"), &link).unwrap();
        assert!(matches!(open_user_config_dir(&link), Err(DirRefusal::UnsafeParent)));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// What counts is the directory the link leads to, not the one the link sits in.
    #[test]
    fn a_link_is_judged_by_where_it_leads() {
        let base = config_scratch("cfg-link-holder");
        real_config_dir(&base.join("dotfiles").join("eitri"));
        set_mode(&base.join("dotfiles"), 0o700);
        let shared = base.join("shared");
        real_config_dir(&shared);
        set_mode(&shared, 0o775);
        let link = shared.join("eitri");
        std::os::unix::fs::symlink(base.join("dotfiles").join("eitri"), &link).unwrap();
        assert!(open_user_config_dir(&link).is_ok());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_missing_directory_is_made_0700_whatever_the_umask() {
        crate::test_umask::with_umask(
            concat!(module_path!(), "::a_missing_directory_is_made_0700_whatever_the_umask"),
            0o277,
            || {
                let base = config_scratch("cfg-made");
                let dir = base.join("eitri");
                let opened = open_user_config_dir(&dir).unwrap();
                assert_eq!(mode(&dir), 0o700);
                assert_eq!(inode_of(opened), dir.metadata().unwrap().ino());
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// 0477 leaves a new directory at 0300: nothing can open it until its mode is set, and it would stay
    /// that way for every later attempt. The file written there keeps what the umask leaves of its mode.
    #[test]
    fn a_missing_directory_is_made_0700_when_the_umask_takes_the_owners_read_bit() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::a_missing_directory_is_made_0700_when_the_umask_takes_the_owners_read_bit"
            ),
            0o477,
            || {
                let base = config_scratch("cfg-made-unreadable");
                let dir = base.join("eitri");
                let opened = open_user_config_dir(&dir).unwrap();
                assert_eq!(mode(&dir), 0o700);
                create_new_file_at(opened.as_fd(), "init.lua", b"-- lines\n", 0o644).unwrap();
                assert_eq!(mode(&dir.join("init.lua")), 0o200);
                assert_eq!(names_in(&dir), ["init.lua"]);
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// The same for a whole private tree, at the umask that leaves the owner nothing: each level has to
    /// be usable before the next is made inside it.
    #[test]
    fn a_umask_that_takes_every_owner_bit_still_gives_0700_and_0600() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::a_umask_that_takes_every_owner_bit_still_gives_0700_and_0600"
            ),
            0o777,
            || {
                let base = config_scratch("umask-none");
                let root = base.join("eitri");
                let dir = root.join("conversations").join("abc");
                let file = dir.join("record.json");
                create_private_dir_all(&dir, &root)
                    .and_then(|()| write_private(&file, b"{}"))
                    .unwrap();
                assert_eq!(mode(&root), 0o700);
                assert_eq!(mode(&root.join("conversations")), 0o700);
                assert_eq!(mode(&dir), 0o700);
                assert_eq!(mode(&file), 0o600);
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    #[test]
    fn a_missing_parent_is_refused() {
        let base = config_scratch("cfg-no-parent");
        let dir = base.join("nowhere").join("eitri");
        assert!(matches!(open_user_config_dir(&dir), Err(DirRefusal::MissingParent)));
        assert!(matches!(probe_user_config_dir(&dir), Err(DirRefusal::MissingParent)));
        assert!(!base.join("nowhere").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_probe_creates_nothing() {
        let base = config_scratch("cfg-probe");
        let dir = base.join("eitri");
        assert!(matches!(probe_user_config_dir(&dir), Ok(None)));
        assert!(!dir.exists());
        real_config_dir(&dir);
        assert!(matches!(probe_user_config_dir(&dir), Ok(Some(_))));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Anything but a directory at the name is refused without being opened, so a FIFO cannot hold the
    /// call up.
    #[test]
    fn a_file_or_a_fifo_at_the_directory_name_is_refused_without_blocking() {
        let base = config_scratch("cfg-file");
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(open_user_config_dir(&file), Err(DirRefusal::NotADirectory)));
        let fifo = base.join("fifo");
        make_fifo(&fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(matches!(probe_user_config_dir(&path), Err(DirRefusal::NotADirectory)));
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the probe blocked on a FIFO");
        assert!(refused);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Each refusal names its reason on one line, for a log.
    #[test]
    fn every_refusal_says_why_on_one_line() {
        for refusal in [
            DirRefusal::NotADirectory,
            DirRefusal::NotOwned,
            DirRefusal::Writable,
            DirRefusal::UnsafeParent,
            DirRefusal::SymlinkChain,
            DirRefusal::MissingParent,
            DirRefusal::Io(std::io::Error::from_raw_os_error(libc::EACCES)),
        ] {
            let said = refusal.to_string();
            assert!(!said.is_empty() && !said.contains('\n'), "{refusal:?}: {said}");
        }
    }

    fn new_config_dir(label: &str) -> (std::path::PathBuf, std::path::PathBuf, OwnedFd) {
        let base = config_scratch(label);
        let dir = base.join("eitri");
        let handle = open_user_config_dir(&dir).unwrap();
        (base, dir, handle)
    }

    #[test]
    fn the_file_appears_whole() {
        crate::test_umask::with_umask(concat!(module_path!(), "::the_file_appears_whole"), 0o022, || {
            let (base, dir, handle) = new_config_dir("new-whole");
            create_new_file_at(handle.as_fd(), "init.lua", b"-- lines\n", 0o644).unwrap();
            assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), b"-- lines\n");
            assert_eq!(mode(&dir.join("init.lua")), 0o644);
            assert_eq!(names_in(&dir), ["init.lua"], "no temporary is left");
            std::fs::remove_dir_all(&base).unwrap();
        });
    }

    #[test]
    fn an_existing_file_is_exists_and_untouched() {
        let (base, dir, handle) = new_config_dir("new-existing");
        std::fs::write(dir.join("init.lua"), b"mine").unwrap();
        let result = create_new_file_at(handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::Exists)), "{result:?}");
        assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), b"mine");
        assert_eq!(names_in(&dir), ["init.lua"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A link at the name counts as a file there, dangling or not; nothing is written through it.
    #[test]
    fn a_symlink_at_the_name_is_exists_and_the_target_untouched() {
        let (base, dir, handle) = new_config_dir("new-link");
        let target = base.join("elsewhere.lua");
        std::fs::write(&target, b"mine").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("init.lua")).unwrap();
        let result = create_new_file_at(handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::Exists)), "{result:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"mine");
        std::fs::remove_file(dir.join("init.lua")).unwrap();
        let dangling = base.join("nothing.lua");
        std::os::unix::fs::symlink(&dangling, dir.join("init.lua")).unwrap();
        let result = create_new_file_at(handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::Exists)), "{result:?}");
        assert!(!dangling.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_fifo_at_the_name_is_exists_and_does_not_block() {
        let (base, dir, handle) = new_config_dir("new-fifo");
        make_fifo(&dir.join("init.lua"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = create_new_file_at(handle.as_fd(), "init.lua", b"theirs", 0o644);
            let _ = tx.send(matches!(result, Err(CreateRefusal::Exists)));
        });
        let exists = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("creating the file blocked on a FIFO");
        assert!(exists);
        assert_eq!(names_in(&dir), ["init.lua"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn two_threads_racing_one_wins_one_is_exists() {
        let base = config_scratch("new-race");
        for round in 0..50 {
            let dir = base.join(format!("eitri-{round}"));
            let handle = open_user_config_dir(&dir).unwrap();
            let start = std::sync::Barrier::new(2);
            let attempt = |bytes: &'static [u8]| {
                start.wait();
                create_new_file_at(handle.as_fd(), "init.lua", bytes, 0o644)
            };
            let (first, second) = std::thread::scope(|scope| {
                let first = scope.spawn(|| attempt(b"first"));
                let second = scope.spawn(|| attempt(b"second"));
                (first.join().unwrap(), second.join().unwrap())
            });
            let wanted: &[u8] = match (&first, &second) {
                (Ok(()), Err(CreateRefusal::Exists)) => b"first",
                (Err(CreateRefusal::Exists), Ok(())) => b"second",
                other => panic!("round {round}: {other:?}"),
            };
            assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), wanted, "round {round}");
            assert_eq!(names_in(&dir), ["init.lua"], "round {round}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The temporary file is created with whatever mode the umask leaves, and its descriptor still
    /// writes: the umask decides the mode of the user's file, never how much of it arrives.
    #[test]
    fn umask_0277_still_writes_the_whole_file() {
        crate::test_umask::with_umask(
            concat!(module_path!(), "::umask_0277_still_writes_the_whole_file"),
            0o277,
            || {
                let (base, dir, handle) = new_config_dir("new-umask");
                let bytes = vec![b'x'; 1 << 20];
                create_new_file_at(handle.as_fd(), "init.lua", &bytes, 0o644).unwrap();
                assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), bytes);
                assert_eq!(mode(&dir.join("init.lua")), 0o400);
                assert_eq!(names_in(&dir), ["init.lua"]);
                std::fs::remove_dir_all(&base).unwrap();
            },
        );
    }

    /// The real calls, but for the errors a test asks for.
    struct Scripted {
        link: Option<i32>,
        rename: Option<i32>,
    }

    impl LinkCalls for Scripted {
        fn link(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()> {
            match self.link {
                Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
                None => SystemCalls.link(dir, from, to),
            }
        }

        fn rename_noreplace(&self, dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> std::io::Result<()> {
            match self.rename {
                Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
                None => SystemCalls.rename_noreplace(dir, from, to),
            }
        }
    }

    /// Only Linux has the call: every other target reports `ENOSYS`, the arm the next test covers.
    #[cfg(target_os = "linux")]
    #[test]
    fn eperm_from_link_takes_the_rename_path() {
        for errno in [libc::EPERM, libc::EOPNOTSUPP, libc::ENOSYS] {
            let (base, dir, handle) = new_config_dir("new-rename");
            let calls = Scripted {
                link: Some(errno),
                rename: None,
            };
            create_new_file_with(&calls, handle.as_fd(), "init.lua", b"-- lines\n", 0o644).unwrap();
            assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), b"-- lines\n");
            assert_eq!(names_in(&dir), ["init.lua"], "no temporary is left");
            std::fs::remove_dir_all(&base).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_rename_never_replaces_a_file() {
        let (base, dir, handle) = new_config_dir("new-rename-real");
        std::fs::write(dir.join("init.lua"), b"mine").unwrap();
        let calls = Scripted {
            link: Some(libc::EPERM),
            rename: None,
        };
        let result = create_new_file_with(&calls, handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::Exists)), "{result:?}");
        assert_eq!(std::fs::read(dir.join("init.lua")).unwrap(), b"mine");
        assert_eq!(names_in(&dir), ["init.lua"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn eexist_on_the_rename_path_is_exists() {
        let (base, dir, handle) = new_config_dir("new-rename-exists");
        let calls = Scripted {
            link: Some(libc::EPERM),
            rename: Some(libc::EEXIST),
        };
        let result = create_new_file_with(&calls, handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::Exists)), "{result:?}");
        assert!(names_in(&dir).is_empty(), "the temporary is gone");
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn refused_rename_leaves_nothing(errno: i32) {
        let (base, dir, handle) = new_config_dir("new-rename-refused");
        let calls = Scripted {
            link: Some(libc::EPERM),
            rename: Some(errno),
        };
        let result = create_new_file_with(&calls, handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(matches!(result, Err(CreateRefusal::CannotCreateSafely)), "{result:?}");
        assert!(names_in(&dir).is_empty(), "no init.lua and no temporary");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn einval_from_rename_is_cannot_create_safely_and_leaves_nothing() {
        refused_rename_leaves_nothing(libc::EINVAL);
    }

    #[test]
    fn enosys_from_rename_is_the_same() {
        refused_rename_leaves_nothing(libc::ENOSYS);
    }

    #[test]
    fn any_other_link_error_stops_there_and_leaves_nothing() {
        let (base, dir, handle) = new_config_dir("new-link-error");
        let calls = Scripted {
            link: Some(libc::EIO),
            rename: Some(libc::EINVAL),
        };
        let result = create_new_file_with(&calls, handle.as_fd(), "init.lua", b"theirs", 0o644);
        assert!(
            matches!(&result, Err(CreateRefusal::Io(e)) if e.raw_os_error() == Some(libc::EIO)),
            "{result:?}"
        );
        assert!(names_in(&dir).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The name is one entry of the directory the handle holds, never a way out of it.
    #[test]
    fn a_name_that_is_not_one_entry_is_refused() {
        let (base, dir, handle) = new_config_dir("new-name");
        for name in ["", ".", "..", "../init.lua", "a/b", "nul\0"] {
            let result = create_new_file_at(handle.as_fd(), name, b"x", 0o644);
            assert!(matches!(result, Err(CreateRefusal::Io(_))), "{name:?}: {result:?}");
        }
        assert!(names_in(&dir).is_empty());
        assert_eq!(names_in(&base), ["eitri"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The control: a directory this user made is accepted when it already exists.
    #[test]
    fn an_existing_directory_of_ours_is_accepted() {
        let base = scratch("ours");
        let root = base.join("eitri");
        std::fs::create_dir(&root).unwrap();
        create_private_dir_all(&root.join("a"), &root).unwrap();
        create_private_dir_all(&root.join("a"), &root).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A scratch directory and a handle on it. The mode is set first: under a narrow umask the scratch
    /// itself comes out without the owner's bits.
    fn scratch_handle(label: &str) -> (std::path::PathBuf, std::fs::File) {
        let dir = scratch(label);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let handle = open_dir(&dir).unwrap();
        (dir, handle)
    }

    /// The directory is made 0700 and the handle is that directory: what is put through it lands there.
    fn a_missing_child_comes_out_0700() {
        let (base, handle) = scratch_handle("child-missing");
        let child = ensure_child_dir(&handle, OsStr::new("ide")).unwrap();
        assert_eq!(mode(&base.join("ide")), 0o700);
        publish_private_file(&child, OsStr::new("1.lock"), b"{}").unwrap();
        assert_eq!(names_in(&base.join("ide")), ["1.lock"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn ensure_child_dir_makes_a_missing_one_0700() {
        a_missing_child_comes_out_0700();
    }

    /// 0277 would leave 0500, in which nothing can be created.
    #[test]
    fn ensure_child_dir_makes_a_missing_one_0700_under_a_narrow_umask() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::ensure_child_dir_makes_a_missing_one_0700_under_a_narrow_umask"
            ),
            0o277,
            a_missing_child_comes_out_0700,
        );
    }

    /// 0477 would leave 0300, on which no handle can be opened until the mode is set.
    #[test]
    fn ensure_child_dir_makes_a_missing_one_0700_under_a_umask_without_the_read_bit() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::ensure_child_dir_makes_a_missing_one_0700_under_a_umask_without_the_read_bit"
            ),
            0o477,
            a_missing_child_comes_out_0700,
        );
    }

    /// The directory is another program's, so an existing one keeps the mode it has, 0750 and 0500 alike.
    #[test]
    fn ensure_child_dir_keeps_an_existing_mode() {
        for kept in [0o750, 0o755, 0o500] {
            let (base, handle) = scratch_handle("child-kept");
            let child = base.join("ide");
            std::fs::create_dir(&child).unwrap();
            set_mode(&child, kept);
            ensure_child_dir(&handle, OsStr::new("ide")).unwrap();
            assert_eq!(mode(&child), kept, "the mode of a directory this call did not make");
            std::fs::remove_dir_all(&base).unwrap();
        }
    }

    /// Runs `ensure_child_dir` with the directory it just made swapped for one of mode `swapped_in`, as
    /// another process of this user could, and returns its answer and the mode left at the name.
    fn with_a_swap_after_the_mkdir(label: &str, swapped_in: u32) -> (std::io::Result<std::fs::File>, u32) {
        let (base, handle) = scratch_handle(label);
        let child = base.join("ide");
        let swap = {
            let child = child.clone();
            move |_: &Path| {
                std::fs::remove_dir(&child).unwrap();
                std::fs::create_dir(&child).unwrap();
                set_mode(&child, swapped_in);
            }
        };
        AFTER_MKDIR.with(|hook| *hook.borrow_mut() = Some(Box::new(swap)));
        let answer = ensure_child_dir(&handle, OsStr::new("ide"));
        AFTER_MKDIR.with(|hook| *hook.borrow_mut() = None);
        let left = mode(&child);
        set_mode(&child, 0o700);
        std::fs::remove_dir_all(&base).unwrap();
        (answer, left)
    }

    /// Group bits can only have come from something else: `mkdirat` with 0700 never leaves any.
    #[test]
    fn a_directory_swapped_in_after_the_mkdir_keeps_its_mode() {
        let (answer, left) = with_a_swap_after_the_mkdir("child-swapped", 0o750);
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(left, 0o750, "the mode of a directory this call did not make");
    }

    /// The owner cannot read this one, so no handle opens on it, and the by-name set is what would change it.
    #[test]
    fn a_swapped_in_directory_no_handle_opens_on_is_refused_and_left() {
        if euid() == 0 {
            return;
        }
        let (answer, left) = with_a_swap_after_the_mkdir("child-swapped-unreadable", 0o350);
        let refused = answer.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert_eq!(left, 0o350, "the mode of a directory this call did not make");
    }

    /// A link to a directory that would pass is refused all the same, and a link to nothing makes nothing.
    #[test]
    fn ensure_child_dir_refuses_a_link() {
        let (base, handle) = scratch_handle("child-link");
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o700);
        std::os::unix::fs::symlink(&target, base.join("ide")).unwrap();
        std::os::unix::fs::symlink(base.join("nowhere"), base.join("dangling")).unwrap();
        for name in ["ide", "dangling"] {
            let refused = ensure_child_dir(&handle, OsStr::new(name)).unwrap_err();
            assert_eq!(
                refused.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{name}: {refused}"
            );
        }
        assert!(names_in(&target).is_empty());
        assert!(!base.join("nowhere").exists(), "a dangling link was made real");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// `/usr/lib` is root's and readable, so only the owner test refuses it.
    #[test]
    fn ensure_child_dir_refuses_another_users_directory() {
        if euid() == 0 {
            return;
        }
        let usr = open_dir(Path::new("/usr")).unwrap();
        let refused = ensure_child_dir(&usr, OsStr::new("lib")).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
    }

    fn assert_refused_and_left_alone(label: &str, mode_of_child: u32) {
        let (base, handle) = scratch_handle(label);
        let child = base.join("ide");
        std::fs::create_dir(&child).unwrap();
        set_mode(&child, mode_of_child);
        let refused = ensure_child_dir(&handle, OsStr::new("ide")).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        let left = child.symlink_metadata().unwrap().mode() & 0o7777;
        assert_eq!(left, mode_of_child, "a refused directory is not fixed");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn ensure_child_dir_refuses_a_group_writable_one() {
        assert_refused_and_left_alone("child-group", 0o770);
    }

    /// The sticky bit keeps others from renaming entries, not from adding them, so it changes nothing here.
    #[test]
    fn ensure_child_dir_refuses_a_world_writable_one() {
        assert_refused_and_left_alone("child-world", 0o707);
        assert_refused_and_left_alone("child-world-sticky", 0o1777);
    }

    /// A FIFO at the name is not opened for reading, which would wait for a writer.
    #[test]
    fn ensure_child_dir_refuses_a_file() {
        let (base, handle) = scratch_handle("child-file");
        std::fs::write(base.join("plain"), b"x").unwrap();
        let fifo = std::ffi::CString::new(base.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for name in ["plain", "fifo"] {
                let _ = tx.send(ensure_child_dir(&handle, OsStr::new(name)).map(|_| ()));
            }
        });
        for _ in 0..2 {
            let refused = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("ensure_child_dir blocked on what is at the name")
                .unwrap_err();
            assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        }
        assert_eq!(std::fs::read(base.join("plain")).unwrap(), b"x");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// `..` would otherwise hand back the directory above, and `a/b` reach past the one entry asked for.
    #[test]
    fn a_name_that_is_not_one_entry_of_the_handle_is_refused() {
        let (base, handle) = scratch_handle("one-entry");
        let kept = OsStr::new("kept");
        let identity = publish_private_file(&handle, kept, b"x").unwrap();
        for name in ["", ".", "..", "../kept", "a/b", "nul\0"] {
            let name = OsStr::new(name);
            let invalid = std::io::ErrorKind::InvalidInput;
            assert_eq!(ensure_child_dir(&handle, name).unwrap_err().kind(), invalid, "{name:?}");
            assert_eq!(
                publish_private_file(&handle, name, b"x").unwrap_err().kind(),
                invalid,
                "{name:?}"
            );
            assert_eq!(
                remove_if_same(&handle, name, identity).unwrap_err().kind(),
                invalid,
                "{name:?}"
            );
        }
        assert_eq!(names_in(&base), ["kept"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A home is judged by nobody here: a link to one opens, and a file is still no directory.
    #[test]
    fn open_dir_follows_a_link_and_refuses_a_file() {
        let (base, _handle) = scratch_handle("open-dir");
        let home = base.join("home");
        std::fs::create_dir(&home).unwrap();
        set_mode(&home, 0o777);
        std::os::unix::fs::symlink(&home, base.join("link")).unwrap();
        let held = open_dir(&base.join("link")).unwrap();
        publish_private_file(&held, OsStr::new("through"), b"x").unwrap();
        assert_eq!(names_in(&home), ["through"]);
        assert_eq!(mode(&home), 0o777, "opening a home changes nothing");
        let file = home.join("through");
        assert_eq!(open_dir(&file).unwrap_err().kind(), std::io::ErrorKind::NotADirectory);
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn a_published_file_is_0600_and_named_by_its_identity() {
        let (base, handle) = scratch_handle("publish-umask");
        let identity = publish_private_file(&handle, OsStr::new("1234.lock"), b"{\"pid\":1}").unwrap();
        let path = base.join("1234.lock");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"pid\":1}");
        let meta = path.symlink_metadata().unwrap();
        let named = FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        assert_eq!(identity, named);
        assert_eq!(names_in(&base), ["1234.lock"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 0277 would leave 0400, which only the handle the file was made by can still write.
    #[test]
    fn publish_writes_a_0600_file_under_any_umask() {
        crate::test_umask::with_umask(
            concat!(module_path!(), "::publish_writes_a_0600_file_under_any_umask"),
            0o277,
            a_published_file_is_0600_and_named_by_its_identity,
        );
    }

    #[test]
    fn publish_writes_a_0600_file_under_an_open_umask() {
        crate::test_umask::with_umask(
            concat!(module_path!(), "::publish_writes_a_0600_file_under_an_open_umask"),
            0o000,
            a_published_file_is_0600_and_named_by_its_identity,
        );
    }

    /// A reader of `*.lock` never meets the file half-written: until the rename the directory holds the
    /// finished file under a name no such reader takes.
    #[test]
    fn a_temporary_name_not_ending_in_lock_exists_until_the_rename() {
        let (base, handle) = scratch_handle("publish-temporary");
        let seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let (watched, saw) = (base.clone(), seen.clone());
        publish_test_hooks::set_before_rename(move || {
            let names = names_in(&watched);
            assert_eq!(names.len(), 1, "{names:?}");
            let name = &names[0];
            let pid = format!(".1234.lock.{}.", std::process::id());
            assert!(name.starts_with(&pid) && name.ends_with(".tmp"), "{name}");
            assert!(!name.ends_with(".lock"), "{name}");
            assert_eq!(std::fs::read(watched.join(name)).unwrap(), b"whole");
            assert_eq!(mode(&watched.join(name)), 0o600);
            saw.set(true);
        });
        publish_private_file(&handle, OsStr::new("1234.lock"), b"whole").unwrap();
        assert!(seen.get(), "the hook did not run");
        assert_eq!(names_in(&base), ["1234.lock"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A window that ended without cleaning up left its lock for this port; the new file takes the name
    /// and the old mode goes with the old file.
    #[test]
    fn publish_replaces_a_dead_processs_file() {
        let (base, handle) = scratch_handle("publish-replace");
        let path = base.join("1234.lock");
        std::fs::write(&path, b"a dead window's").unwrap();
        set_mode(&path, 0o644);
        let old = path.symlink_metadata().unwrap().ino();
        let identity = publish_private_file(&handle, OsStr::new("1234.lock"), b"new").unwrap();
        assert_ne!(identity.ino, old);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(names_in(&base), ["1234.lock"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The rename replaces the link itself: what it pointed at is not written.
    #[test]
    fn publish_replaces_a_link_without_writing_through_it() {
        let (base, handle) = scratch_handle("publish-link");
        let victim = base.join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, base.join("1234.lock")).unwrap();
        publish_private_file(&handle, OsStr::new("1234.lock"), b"new").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        let meta = base.join("1234.lock").symlink_metadata().unwrap();
        assert!(meta.file_type().is_file());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A directory at the name lets the temporary be written and fails the rename.
    #[test]
    fn a_failed_publish_leaves_no_temporary_name() {
        let (base, handle) = scratch_handle("publish-fail");
        std::fs::create_dir(base.join("1234.lock")).unwrap();
        let seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let (watched, saw) = (base.clone(), seen.clone());
        publish_test_hooks::set_before_rename(move || {
            assert_eq!(names_in(&watched).len(), 2, "the temporary was not written");
            saw.set(true);
        });
        publish_private_file(&handle, OsStr::new("1234.lock"), b"x").unwrap_err();
        assert!(seen.get(), "the publish stopped before the rename");
        assert_eq!(names_in(&base), ["1234.lock"]);
        assert!(base.join("1234.lock").is_dir());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Another file at the name is left there, a link to the file included, and a name already gone is
    /// no error.
    #[test]
    fn remove_if_same_removes_only_that_file() {
        let (base, handle) = scratch_handle("remove-same");
        let name = OsStr::new("1234.lock");
        let path = base.join("1234.lock");
        let first = publish_private_file(&handle, name, b"first").unwrap();
        let second = publish_private_file(&handle, name, b"second").unwrap();
        assert!(!remove_if_same(&handle, name, first).unwrap(), "a file renamed over it");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        std::fs::rename(&path, base.join("moved")).unwrap();
        std::os::unix::fs::symlink(base.join("moved"), &path).unwrap();
        assert!(!remove_if_same(&handle, name, second).unwrap(), "a link to it");
        assert_eq!(names_in(&base), ["1234.lock", "moved"]);

        std::fs::remove_file(&path).unwrap();
        std::fs::rename(base.join("moved"), &path).unwrap();
        assert!(remove_if_same(&handle, name, second).unwrap());
        assert!(names_in(&base).is_empty());
        assert!(
            !remove_if_same(&handle, name, second).unwrap(),
            "nothing left at the name"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
