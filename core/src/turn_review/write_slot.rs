//! The project's one write slot, shared by every Eitri window that has the project open.
//!
//! A window lets one revert, undo or recovery write at a time, and none while a review message is
//! being made from the disk ([`super::flow`]'s own slot). That slot lives in one process, and a
//! revert runs its last look and its write after letting go of the shadow's shared lock, so two
//! windows could each write the same file at once and the later rename would silently undo the
//! other's revert while both report success. So every window also takes an `flock` on
//! `<review dir>/write.lock`: a write job exclusively, from the first thing its worker does until
//! after its write; a review send shared, around its read of the disk. Neither ever waits: a slot
//! someone else holds refuses the job with the note one window gives today, naming the other
//! window.
//!
//! Each take opens the file afresh, so two takes in one process exclude each other as two windows
//! do (`flock` belongs to an open file description, not to a process). A hold lets go with
//! `LOCK_UN` before closing, so a child forked meanwhile, which shares the description until it
//! execs, does not keep the slot taken.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

use super::shadow::private_root;

/// The slot's file, beside `running/` and `windows/`.
const SLOT_FILE: &str = "write.lock";

/// How many times a take is tried again because the name stopped naming the file it locked.
const ATTEMPTS: usize = 8;

/// Why the slot was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotBusy {
    /// Another window is writing a revert, an undo or a recovery.
    Writing,
    /// Another window is making a review message from the disk.
    Reading,
    /// The slot's file could not be opened or locked, so nobody can say the project is free.
    Unavailable(String),
}

impl fmt::Display for SlotBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlotBusy::Writing => {
                f.write_str("a revert is still being written in another Eitri window; try again in a moment")
            }
            SlotBusy::Reading => {
                f.write_str("the review message is still being prepared in another Eitri window; try again in a moment")
            }
            SlotBusy::Unavailable(why) => write!(f, "the project's write slot could not be taken: {why}"),
        }
    }
}

impl std::error::Error for SlotBusy {}

/// One window's way to the slot: its own descriptor of the review directory, from which every take
/// opens the slot's file again.
pub struct WriteSlot {
    dir: File,
    shown: PathBuf,
}

/// A taken slot, let go when dropped.
#[must_use = "the slot is let go as soon as the hold is dropped"]
pub struct SlotHold {
    file: File,
}

impl Drop for SlotHold {
    fn drop(&mut self) {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl fmt::Debug for SlotHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlotHold")
    }
}

fn slot_name() -> std::ffi::CString {
    std::ffi::CString::new(SLOT_FILE).expect("a constant with no NUL")
}

/// The calling user's effective uid.
fn own_uid() -> libc::uid_t {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn fstat(file: &File) -> io::Result<libc::stat> {
    // SAFETY: `stat` is plain data, filled in by `fstat` before it is read.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the call and `st` is valid for writing.
    if unsafe { libc::fstat(file.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what)
}

impl WriteSlot {
    /// Opens `review_dir` (made 0700 when missing) for the slot. A directory that is a symlink, or
    /// not this user's, is refused. The file itself is opened by each take.
    pub fn open(review_dir: &Path) -> io::Result<WriteSlot> {
        agent::private_fs::create_private_dir_all(review_dir, private_root(review_dir))?;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(review_dir)?;
        let st = fstat(&dir)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR || st.st_uid != own_uid() {
            return Err(invalid(format!(
                "{} is not a directory of this user's own",
                review_dir.display()
            )));
        }
        Ok(WriteSlot {
            dir,
            shown: review_dir.join(SLOT_FILE),
        })
    }

    /// [`WriteSlot::open`], a failure being a slot nobody can say is free.
    pub fn open_or_busy(review_dir: &Path) -> Result<WriteSlot, SlotBusy> {
        WriteSlot::open(review_dir).map_err(|e| SlotBusy::Unavailable(format!("{}: {e}", review_dir.display())))
    }

    /// Takes the slot for a write: refused while anyone holds it at all.
    pub fn try_exclusive(&self) -> Result<SlotHold, SlotBusy> {
        match self.take(libc::LOCK_EX)? {
            Some(hold) => Ok(hold),
            // Who holds it decides the note: a shared hold that can be joined is a message being
            // made, anything else a write. The probe is itself a shared hold for a moment: a write
            // another window tries meanwhile is refused with the message note, and a write that
            // let go between the two takes is reported as a message being made though the slot is
            // free by then. Only the note's wording can be wrong: the job is refused as it would
            // have been a moment earlier, and trying again finds the slot as it is.
            None => match self.take(libc::LOCK_SH)? {
                Some(_probe) => Err(SlotBusy::Reading),
                None => Err(SlotBusy::Writing),
            },
        }
    }

    /// Takes the slot for a read of the disk: refused only while a write holds it.
    pub fn try_shared(&self) -> Result<SlotHold, SlotBusy> {
        self.take(libc::LOCK_SH)?.ok_or(SlotBusy::Writing)
    }

    /// `Some` with the lock taken, `None` when someone holds it in a way that excludes `operation`.
    fn take(&self, operation: libc::c_int) -> Result<Option<SlotHold>, SlotBusy> {
        let unavailable = |e: io::Error| SlotBusy::Unavailable(format!("{}: {e}", self.shown.display()));
        for _ in 0..ATTEMPTS {
            let file = self.open_file().map_err(unavailable)?;
            loop {
                // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no
                // pointers.
                if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
                    break;
                }
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EWOULDBLOCK) => return Ok(None),
                    _ => return Err(unavailable(err)),
                }
            }
            let hold = SlotHold { file };
            // A file unlinked and made again between the open and the lock is not the one the
            // other window opens next: the lock would exclude nobody.
            if self.names(&hold.file).map_err(unavailable)? {
                return Ok(Some(hold));
            }
        }
        Err(SlotBusy::Unavailable(format!(
            "{} kept being replaced while it was locked",
            self.shown.display()
        )))
    }

    /// A fresh open file description of the slot's file, made 0600 when missing. A symlink, a file
    /// that is not regular or not this user's is refused.
    ///
    /// The file is set to exactly 0600 whatever the umask: one that takes the owner's write bit
    /// away (0277) would create it 0400, and every later take, which opens it for writing, would
    /// be refused for good. A file already left that way is opened for reading once to put its
    /// mode right.
    fn open_file(&self) -> io::Result<File> {
        let file = match self.open_at(libc::O_RDWR | libc::O_CREAT) {
            Err(e) if e.raw_os_error() == Some(libc::EACCES) => {
                let repaired = self.open_at(libc::O_RDONLY)?;
                self.make_private(&repaired)?;
                drop(repaired);
                self.open_at(libc::O_RDWR | libc::O_CREAT)?
            }
            other => other?,
        };
        self.make_private(&file)?;
        Ok(file)
    }

    /// `openat` of the slot's name with `access` and the flags every open of it takes.
    fn open_at(&self, access: libc::c_int) -> io::Result<File> {
        let name = slot_name();
        loop {
            // SAFETY: the directory descriptor is open for the call and `name` is NUL-terminated.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    name.as_ptr(),
                    access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
                    agent::private_fs::PRIVATE_FILE_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` was just opened here and nothing else owns it.
                return Ok(unsafe { File::from_raw_fd(fd) });
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }
    }

    /// Refuses `file` unless it is a regular file of this user's, then sets it to 0600 if it is
    /// not already.
    fn make_private(&self, file: &File) -> io::Result<()> {
        let st = fstat(file)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG || st.st_uid != own_uid() {
            return Err(invalid(format!(
                "{} is not a regular file of this user's own",
                self.shown.display()
            )));
        }
        let wanted = agent::private_fs::PRIVATE_FILE_MODE;
        if st.st_mode & 0o7777 != wanted as libc::mode_t {
            // On the descriptor, never the name: nothing is looked up again.
            file.set_permissions(std::fs::Permissions::from_mode(wanted))?;
        }
        Ok(())
    }

    /// Whether the slot's name in the directory still names `file`.
    fn names(&self, file: &File) -> io::Result<bool> {
        let held = fstat(file)?;
        let name = slot_name();
        // SAFETY: `stat` is plain data, filled in by `fstatat` before it is read.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the directory descriptor is open, `name` is NUL-terminated and `st` is writable.
        let r = unsafe { libc::fstatat(self.dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if r != 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENOENT) => Ok(false),
                _ => Err(err),
            };
        }
        Ok(st.st_dev == held.st_dev && st.st_ino == held.st_ino)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A directory of the test's own, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("eitri-write-slot-{name}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }

        fn review_dir(&self) -> PathBuf {
            let dir = self.0.join("state/eitri/review/0123456789abcdef");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn two_holds_in_one_process_exclude_each_other() {
        let scratch = Scratch::new("one-process");
        let dir = scratch.review_dir();
        // Two windows' worth of slots, and a hold of each kind against each.
        let mine = WriteSlot::open(&dir).unwrap();
        let other = WriteSlot::open(&dir).unwrap();

        let writing = mine.try_exclusive().unwrap();
        assert_eq!(other.try_exclusive().unwrap_err(), SlotBusy::Writing);
        assert_eq!(other.try_shared().unwrap_err(), SlotBusy::Writing);
        assert_eq!(
            mine.try_exclusive().unwrap_err(),
            SlotBusy::Writing,
            "the same slot does not let a second write in either"
        );
        drop(writing);

        let reading = mine.try_shared().unwrap();
        let reading_too = other.try_shared().expect("two reads of the disk may run at once");
        assert_eq!(other.try_exclusive().unwrap_err(), SlotBusy::Reading);
        drop(reading);
        assert_eq!(
            mine.try_exclusive().unwrap_err(),
            SlotBusy::Reading,
            "one read still out keeps writes away"
        );
        drop(reading_too);

        let free = other.try_exclusive().expect("a slot nobody holds is free");
        drop(free);
        assert!(SlotBusy::Writing.to_string().contains("another Eitri window"));
        assert!(SlotBusy::Reading.to_string().contains("another Eitri window"));
    }

    #[test]
    fn a_slot_file_left_without_its_write_bit_is_put_right_and_taken() {
        let scratch = Scratch::new("read-only");
        let dir = scratch.review_dir();
        // What a take under a umask of 0277 used to leave behind.
        std::fs::write(dir.join(SLOT_FILE), b"").unwrap();
        std::fs::set_permissions(dir.join(SLOT_FILE), std::fs::Permissions::from_mode(0o400)).unwrap();
        let slot = WriteSlot::open(&dir).unwrap();
        let writing = slot
            .try_exclusive()
            .expect("a 0400 slot file does not lock the project for good");
        let mode = std::fs::metadata(dir.join(SLOT_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            WriteSlot::open(&dir).unwrap().try_shared().unwrap_err(),
            SlotBusy::Writing
        );
        drop(writing);
        drop(slot.try_shared().unwrap());
    }

    #[test]
    fn the_slot_file_is_private_and_a_planted_symlink_is_refused() {
        let scratch = Scratch::new("private");
        let dir = scratch.review_dir();
        let slot = WriteSlot::open(&dir).unwrap();
        drop(slot.try_exclusive().unwrap());
        let mode = std::fs::metadata(dir.join(SLOT_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        std::fs::remove_file(dir.join(SLOT_FILE)).unwrap();
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::write(&elsewhere, b"").unwrap();
        symlink(&elsewhere, dir.join(SLOT_FILE)).unwrap();
        assert!(matches!(slot.try_exclusive(), Err(SlotBusy::Unavailable(_))));
        assert!(matches!(slot.try_shared(), Err(SlotBusy::Unavailable(_))));

        let linked_dir = scratch.0.join("linked-review");
        symlink(&dir, &linked_dir).unwrap();
        assert!(
            WriteSlot::open(&linked_dir).is_err(),
            "a review directory reached by a link"
        );
    }
}
