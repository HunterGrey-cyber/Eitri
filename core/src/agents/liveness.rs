//! Whether the process that wrote an agents file still runs, and removing what a dead one left behind.
//!
//! A writer holds an exclusive `flock` on `<instance>.lock` for its whole life, so the lock is the final word: a
//! reader that can take it knows the writer is gone, and only the reader that took it removes the files. A
//! `pidfd` is how a reader hears of the death without polling: it becomes readable when the process exits.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use agent::private_fs::euid;

use super::publish::{is_instance, process_start_time};

/// What a reader learned about a writer.
#[derive(Debug)]
pub enum Alive {
    /// Running. The `pidfd` that turns readable when it exits, where one could be opened; without one the reader
    /// asks the lock again every `LOCK_POLL_MS`.
    Yes(Option<OwnedFd>),
    No,
}

/// Whether the writer with `pid`, started at `pid_start` (clock ticks since boot, from `/proc/<pid>/stat`), runs.
///
/// A `pidfd` is opened first and the start time read after it, so a `Yes` holds a descriptor on the very process
/// that was checked: a process found at that pid with that start time has been alive since before the descriptor
/// was opened. A pid that is gone, names a later process, or names a process that has exited but is not yet reaped
/// (its `pidfd` already reads as ended), is dead unless the writer's lock is still held: a writer that runs in
/// another pid namespace has a pid this reader cannot see, and the lock is never wrong. Where `pidfd_open` is
/// missing (Linux before 5.3) or refused, the lock alone decides.
pub fn check(pid: u32, pid_start: u64, lock: &Path) -> Alive {
    check_with(pid, pid_start, lock, true)
}

/// [`check`], with the `pidfd` left out when `use_pidfd` is false, as on a kernel without one.
#[doc(hidden)]
pub fn check_with(pid: u32, pid_start: u64, lock: &Path, use_pidfd: bool) -> Alive {
    let by_lock = |pidfd: Option<OwnedFd>| {
        if lock_is_held(lock) {
            Alive::Yes(pidfd)
        } else {
            Alive::No
        }
    };
    if !use_pidfd || pid == 0 || libc::pid_t::try_from(pid).is_err() {
        return by_lock(None);
    }
    match pidfd_open(pid) {
        Ok(pidfd) => match process_start_time(pid) {
            // Ended, and kept as a zombie until its parent collects it: the pid and its start time are still there.
            _ if has_exited(&pidfd) => by_lock(None),
            Some(start) if pid_start != 0 && start == pid_start => Alive::Yes(Some(pidfd)),
            Some(_) if pid_start != 0 => by_lock(None),
            // The writer had no start time to give, or this reader cannot read one: the lock says, and the
            // descriptor still wakes the reader when the process ends.
            _ => by_lock(Some(pidfd)),
        },
        // No process has the pid (`ESRCH`), or there is no `pidfd` here.
        Err(_) => by_lock(None),
    }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: `pidfd_open` takes a pid and a flags word, no pointers; a negative return is an error.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns; descriptors fit in a `c_int`.
    Ok(unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) })
}

/// Whether the process behind `pidfd` has ended, collected or not: its descriptor then reads as ready.
fn has_exited(pidfd: &OwnedFd) -> bool {
    let mut fd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialised entry that lives for the call; a zero timeout never waits.
    unsafe { libc::poll(&mut fd, 1, 0) > 0 }
}

/// Whether `lock` may still be held by its writer: only a lock taken here says it is not. Opened without
/// `O_CREAT` (a missing lock is no writer's), without following a link (a link at the name was never a writer's),
/// and only judged when it is a regular file of this user. Any other failure to open or lock it (out of
/// descriptors, say) tells nothing of the writer, which is kept and asked again later. Opened for writing too, as
/// the writer's own is: over NFS an exclusive `flock` on a read-only descriptor fails.
pub(crate) fn lock_is_held(lock: &Path) -> bool {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(lock)
    {
        Ok(file) => file,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ELOOP)) => return false,
        Err(_) => return true,
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() && meta.uid() == euid() => {}
        _ => return false,
    }
    // Taken here means nobody held it. It is let go before `file` closes: a child another thread is starting shares
    // this open file, and with it the lock, until it execs, so a lock left to the close could still read as held to
    // the next look, and `clean_dead` would leave the files of a writer that is gone.
    if !matches!(try_flock(&file), Ok(true)) {
        return true;
    }
    #[cfg(test)]
    probe_hook::taken(&file);
    // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    false
}

/// A test's look at the descriptor [`lock_is_held`] has just locked, before it lets the lock go.
#[cfg(test)]
mod probe_hook {
    use std::cell::RefCell;
    use std::fs::File;

    type Hook = Box<dyn FnMut(&File)>;

    thread_local! {
        static TAKEN: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(super) fn set(hook: impl FnMut(&File) + 'static) {
        TAKEN.with(|taken| *taken.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn taken(file: &File) {
        TAKEN.with(|taken| {
            if let Some(hook) = taken.borrow_mut().as_mut() {
                hook(file);
            }
        });
    }
}

/// One try at the exclusive lock: `Ok(true)` when taken, `Ok(false)` when another open holds it.
fn try_flock(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) => return Ok(false),
            _ => return Err(e),
        }
    }
}

fn c_name(name: String) -> io::Result<CString> {
    CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// `fstatat` without following a link: `None` when nothing has the name.
fn stat_at(dir: &File, name: &CString) -> io::Result<Option<libc::stat>> {
    // SAFETY: an all-zero `stat` is a valid value for the kernel to overwrite.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is open for the call, `name` is NUL-terminated, and `stat` is valid for writes.
    if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        return Ok(Some(stat));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        _ => Err(e),
    }
}

/// Removes `name` from `dir`: `Ok(false)` when it was already gone.
fn unlink_at(dir: &File, name: &CString) -> io::Result<bool> {
    // SAFETY: `dir` is open for the call and `name` is NUL-terminated and outlives it.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT) => Ok(false),
        _ => Err(e),
    }
}

/// Removes what the dead writer `instance` left: `<instance>.json` and `<instance>.lock` in `agents_dir`, and
/// `ag-<instance>.sock` in `control_dir` when it is a socket of this user. Done only while holding the writer's
/// lock (`LOCK_EX | LOCK_NB`), and only while the lock file at that name is the one locked, so of several readers
/// that find one dead writer exactly one removes it; a writer that still holds its lock is never touched.
/// Returns whether this call removed the writer's file. With no lock file left there is no writer to hold one
/// (instance names are never reused), and what remains is removed.
pub fn clean_dead(agents_dir: &File, control_dir: &File, instance: &str) -> io::Result<bool> {
    if !is_instance(instance) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{instance:?} is not an instance name"),
        ));
    }
    let lock_name = c_name(format!("{instance}.lock"))?;
    let file_name = c_name(format!("{instance}.json"))?;
    let socket_name = c_name(format!("ag-{instance}.sock"))?;

    // SAFETY: `agents_dir` is open for the call and `lock_name` is NUL-terminated and outlives it.
    let raw = unsafe {
        libc::openat(
            agents_dir.as_raw_fd(),
            lock_name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    let lock = if raw >= 0 {
        // SAFETY: a new descriptor nothing else owns.
        Some(unsafe { File::from_raw_fd(raw) })
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) {
            return Err(e);
        }
        None
    };
    if let Some(lock) = &lock {
        let meta = lock.metadata()?;
        if !meta.is_file() || meta.uid() != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{instance}.lock is not a regular file of this user"),
            ));
        }
        if !try_flock(lock)? {
            return Ok(false);
        }
        // Another reader that cleaned first has unlinked the file this descriptor locks.
        // The widths of these two fields differ from one unix to another.
        #[allow(clippy::unnecessary_cast)]
        let named = stat_at(agents_dir, &lock_name)?.map(|s| (s.st_dev as u64, s.st_ino as u64));
        if named != Some((meta.dev(), meta.ino())) {
            return Ok(false);
        }
    }
    let removed = unlink_at(agents_dir, &file_name)?;
    if let Some(stat) = stat_at(control_dir, &socket_name)? {
        if stat.st_mode & libc::S_IFMT == libc::S_IFSOCK && stat.st_uid == euid() {
            unlink_at(control_dir, &socket_name)?;
        }
    }
    if lock.is_some() {
        // Unlinked while still held, so no other reader takes the name for a writer to clean.
        unlink_at(agents_dir, &lock_name)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    /// A child that holds a copy of `fd` until its stdin closes, as a child another thread is starting holds every
    /// descriptor until it execs. It says `held` once it has the copy.
    fn holder_of(fd: libc::c_int) -> Child {
        // Another number than `fd`: `dup2` onto itself would keep close-on-exec, and the copy would go.
        let copy = if fd == 50 { 51 } else { 50 };
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                r#"if [ -e "/proc/self/fd/$1" ]; then echo held; else echo lost; fi; read _; exit 0"#,
                "sh",
                &copy.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        // SAFETY: `dup2` is async-signal-safe and touches only the child's descriptor table.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, copy) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut said = [0u8; 5];
        child.stdout.take().unwrap().read_exact(&mut said).unwrap();
        assert_eq!(&said, b"held\n", "the shell holds a copy of the look's descriptor");
        child
    }

    #[test]
    fn a_look_at_a_free_lock_leaves_it_free_whatever_copies_its_descriptor_has() {
        let dir = agent::state_dirs::test_workspace_dir("liveness-probe");
        let lock = dir.join("0123456789abcdef.lock");
        agent::private_fs::write_private(&lock, b"").unwrap();
        let holders = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let kept = holders.clone();
        probe_hook::set(move |file| kept.borrow_mut().push(holder_of(file.as_raw_fd())));
        assert!(!lock_is_held(&lock), "nobody holds it");
        assert_eq!(holders.borrow().len(), 1, "the look took the lock");
        let again = lock_is_held(&lock);
        for mut holder in holders.borrow_mut().drain(..) {
            drop(holder.stdin.take());
            holder.wait().unwrap();
        }
        assert!(
            !again,
            "the first look's lock lived on in a child's copy of its descriptor"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
