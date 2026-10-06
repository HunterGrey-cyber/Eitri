//! Directories and files only this user can read (v1 hardening Task 9, ruling R5).
//!
//! Everything Eitri keeps under `$XDG_STATE_HOME/eitri/` -- conversation records, stored
//! histories, the prompt history, permission rules, layouts, the empty tab's mode -- used to be
//! created with the process umask's defaults: `0755` directories and `0644` files under the usual
//! umask 022. Where the path down to the state home is traversable by other accounts (Eitri is
//! the first to create `~/.local/state`, or `XDG_STATE_HOME` points somewhere shared), any local
//! user could read every prompt the owner typed (the local-IPC review's finding 8). The rule now:
//! directories 0700, files 0600, and a directory of Eitri's own that an older build left open is
//! tightened when Eitri next writes into it -- `chmod`, never `chown`, and only on a real
//! directory owned by this user.
//!
//! Also the one place `eitri-core` asks "is this ours?" ([`current_uid`]): its stale-instance
//! sweep must skip another user's entries in a shared `TMPDIR` before connecting to anything.
//!
//! Modes are changed with `fchmod` on a handle opened without following a symlink, never by path:
//! the handle is `fstat`ed for its owner first, so what is checked is what is changed. (This also
//! keeps the prost-setter token `wire_guard` forbids out of this crate's production code.)
//!
//! **C3 (v1 hardening).** Tightening `root/a/b/...` by re-opening each successive *path*
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

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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
fn euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether `path` is itself a real directory (a symlink is not followed) owned by this user.
pub fn is_own_real_dir(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Ok(meta) if meta.is_dir() && meta.uid() == current_uid())
}

/// Creates `dir` and every missing directory above it with mode 0700, then tightens `root` and
/// each existing directory from `root` down to `dir` to 0700 where one of them is still open to
/// the group or others -- an older build's `0755`. The walk is by open handle
/// (`openat` from the previous component), never by path, so a symlink anywhere from `root`
/// itself down to `dir`'s parent stops the walk there rather than being resolved through -- see
/// the module doc's C3 section for why that matters and what a symlinked `root` means.
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
    // A `root` of someone else's is refused before anything is created inside it, not only after.
    if dir.starts_with(root) {
        match std::fs::metadata(root) {
            Ok(meta) => refuse_foreign(root, &meta)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)?;
    refuse_foreign_dirs(dir, root)?;
    match dir.strip_prefix(root) {
        Ok(below) => tighten_from_root(root, below),
        Err(_) => tighten_own_dir(dir),
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

/// Sets `dir` to 0700 when it is a real directory owned by this user and still has any group or
/// other bit. Best-effort and silent: a failure leaves the files below it 0600 all the same.
///
/// This opens `dir` by *path*, so -- exactly as before C3 -- only `dir`'s own last path component
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
        return None;
    }
    // SAFETY: `fd` was just returned by the `openat` call above, is a valid open descriptor, and is
    // owned by nothing else -- wrapping it in a `File` is what gives it a `Drop` that closes it.
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// `fchmod`s `handle` to 0700 when it `fstat`s as a directory owned by this user with any group or
/// other bit still set. Shared by [`tighten_own_dir`] and [`tighten_from_root`]'s walk: what is
/// checked here is always exactly the handle that would be changed, never a path re-resolved after
/// the check.
fn tighten_handle(handle: &std::fs::File) {
    let Ok(meta) = handle.metadata() else { return };
    if !meta.is_dir() || meta.uid() != current_uid() || meta.mode() & 0o077 == 0 {
        return;
    }
    let _ = fchmod(handle, PRIVATE_DIR_MODE);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        path.symlink_metadata().unwrap().mode() & 0o777
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nv-private-fs-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Umask 002 is the case the review measured (`0775`/`0664`); the modes here are set
    /// explicitly, so the process umask cannot widen them. The umask is process-wide, so this test
    /// only ever narrows what it widened -- `0o002` for its own creations, restored before asserting.
    #[test]
    fn new_directories_are_0700_and_files_0600_whatever_the_umask() {
        let base = scratch("umask");
        let root = base.join("eitri");
        let dir = root.join("conversations").join("abc");
        let file = dir.join("record.json");
        // SAFETY: `umask` has no preconditions; the old value is restored right after.
        let old = unsafe { libc::umask(0o002) };
        let created = create_private_dir_all(&dir, &root).and_then(|()| write_private(&file, b"{}"));
        unsafe { libc::umask(old) };
        created.unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("conversations")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
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
}
