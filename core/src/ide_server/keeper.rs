//! The lock file's life on the worker: watching it, writing it back, removing it.
//!
//! A `claude`'s sweep or a person's `rm` can take the file while the window lives, and without it nothing attaches.
//! Every put-back starts at the home again, so a directory that went, was replaced or was made unsafe is judged before
//! anything is written, and the file's own name going is reason enough to look. That matters for `rm -rf`: while the
//! judged directory's handle is open the kernel raises no `IN_DELETE_SELF` for it, and the file's delete is what is
//! seen.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::c_int;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::private_fs::FileIdentity;

use super::lock::{self, LockDir};
use super::types::Housekeeping;

/// The least time between two attempts to put the file back, so a `claude`'s sweep and this keeper never spin.
const PACE: Duration = Duration::from_secs(1);

const WATCHED: u32 =
    libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_ONLYDIR;
const NAME_WENT: u32 = libc::IN_DELETE | libc::IN_MOVED_FROM;
const DIRECTORY_WENT: u32 = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED;

/// `struct inotify_event` without its name: `wd`, `mask`, `cookie`, `len`, four bytes each.
const EVENT_HEADER: usize = 16;
/// Room for at least one event with the longest name, which a smaller read would refuse.
const EVENT_BUFFER: usize = 4096;

/// The lock file of one listener, from `start` to `finish`.
pub struct LockKeeper {
    home: Option<PathBuf>,
    pid: u32,
    make_inotify: fn() -> io::Result<OwnedFd>,
    kept: Option<Kept>,
}

/// What exists between a good `start` and `finish`.
struct Kept {
    port: u16,
    name: OsString,
    text: String,
    /// The directory last judged, which is where `finish` looks.
    dir: LockDir,
    /// The file last written, so `finish` leaves any other.
    identity: FileIdentity,
    /// One instance for the keeper's whole life, so `watch_fd` never changes under the worker's `poll`.
    inotify: OwnedFd,
    /// `None` from the moment the directory went until a walk finds one.
    watch: Option<c_int>,
    /// The file may be missing, or the directory may have gone: a walk is due.
    owed: bool,
    last_try: Option<Instant>,
    said_written: bool,
    said_refused: bool,
}

fn make_inotify() -> io::Result<OwnedFd> {
    // SAFETY: `inotify_init1` takes no pointers; a negative return is an error.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// A watch on the very directory `dir` holds. The same directory watched again gives the same descriptor back.
fn add_watch(inotify: BorrowedFd<'_>, dir: &LockDir) -> io::Result<c_int> {
    let path = CString::new(dir.proc_path().as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated and outlives the call, and the kernel does not keep it.
    let watch = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), WATCHED) };
    if watch < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(watch)
}

/// Whether two handles are on one directory. Each handle pins its inode, so neither number can have been reused.
fn same_directory(one: &LockDir, other: &LockDir) -> bool {
    let identity = |dir: &LockDir| {
        let meta = std::fs::metadata(dir.proc_path()).ok()?;
        Some((meta.dev(), meta.ino()))
    };
    matches!((identity(one), identity(other)), (Some(one), Some(other)) if one == other)
}

/// The line for a try that failed. A refusal `start` hands back ends `not attached`, which is not so here: the listener
/// is up and whoever is connected stays connected.
fn refused_line(why: &str) -> String {
    let why = why.strip_suffix("; not attached").unwrap_or(why);
    format!("{why}; the listener stays up, trying again every second")
}

/// What one read of the instance said.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    file_went: bool,
    directory_went: bool,
    /// The queue overflowed: anything may have happened.
    events_lost: bool,
}

/// Looks through the events of one read. An event of a watch since replaced is about a directory no `claude` reads.
fn look_through(mut events: &[u8], watch: Option<c_int>, own_name: &OsStr) -> Seen {
    let mut seen = Seen::default();
    while events.len() >= EVENT_HEADER {
        let word = |at: usize| [events[at], events[at + 1], events[at + 2], events[at + 3]];
        let wd = c_int::from_ne_bytes(word(0));
        let mask = u32::from_ne_bytes(word(4));
        let name_len = u32::from_ne_bytes(word(12)) as usize;
        let end = EVENT_HEADER.saturating_add(name_len).min(events.len());
        let padded = &events[EVENT_HEADER..end];
        let name = padded.split(|byte| *byte == 0).next().unwrap_or_default();
        events = &events[end..];

        if mask & libc::IN_Q_OVERFLOW != 0 {
            seen.events_lost = true;
        } else if Some(wd) == watch {
            seen.directory_went |= mask & DIRECTORY_WENT != 0;
            seen.file_went |= mask & NAME_WENT != 0 && name == own_name.as_bytes();
        }
    }
    seen
}

impl Kept {
    /// Reads until the instance is empty, and notes what is owed.
    fn read_events(&mut self) {
        let mut buffer = [0u8; EVENT_BUFFER];
        loop {
            // SAFETY: reads at most `buffer.len()` bytes into `buffer`, which lives for the call.
            let read = unsafe { libc::read(self.inotify.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            let Ok(read) = usize::try_from(read) else {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            };
            if read == 0 {
                return;
            }
            let seen = look_through(&buffer[..read], self.watch, &self.name);
            if seen.directory_went {
                self.drop_watch();
            }
            self.owed |= seen.file_went || seen.directory_went || seen.events_lost;
        }
    }

    fn drop_watch(&mut self) {
        if let Some(watch) = self.watch.take() {
            // SAFETY: takes no pointers. A watch the kernel dropped already answers `EINVAL`, which changes nothing.
            unsafe { libc::inotify_rm_watch(self.inotify.as_raw_fd(), watch) };
        }
    }

    /// Walks from `home`, watches what it finds, and puts the file there unless something already holds the name.
    /// Whatever fails stays owed, for the next try at the pace.
    fn settle(&mut self, home: Option<&Path>, now: Instant) {
        if self
            .last_try
            .is_some_and(|last| now.saturating_duration_since(last) < PACE)
        {
            return;
        }
        self.last_try = Some(now);
        let dir = match lock::open_lock_dir(home) {
            Ok(dir) => dir,
            Err(why) => return self.say_refused(why),
        };
        // A directory removed while its handle is held keeps its watch, and at the user's limit that watch's slot is
        // the only one the new watch can have. The same directory keeps its watch, and `add_watch` gives it back.
        if !same_directory(&self.dir, &dir) {
            self.drop_watch();
        }
        let watch = match add_watch(self.inotify.as_fd(), &dir) {
            Ok(watch) => watch,
            Err(err) => return self.say_refused(&format!("terminal claude: cannot watch ~/.claude/ide: {err}")),
        };
        // The name is looked at only now, with the watch on, so nothing that happens from here on goes unseen.
        self.watch = Some(watch);
        self.dir = dir;
        if std::fs::symlink_metadata(self.dir.proc_path().join(&self.name)).is_ok() {
            self.owed = false;
            return;
        }
        match lock::publish(&self.dir, self.port, &self.text) {
            Ok(identity) => {
                self.identity = identity;
                self.owed = false;
                if !std::mem::replace(&mut self.said_written, true) {
                    eprintln!("terminal claude: the lock file under ~/.claude/ide went; written again");
                }
            }
            Err(err) => self.say_refused(&format!("terminal claude: cannot write the lock file: {err}")),
        }
    }

    fn say_refused(&mut self, why: &str) {
        if !std::mem::replace(&mut self.said_refused, true) {
            eprintln!("{}", refused_line(why));
        }
    }
}

impl LockKeeper {
    pub fn new(home: Option<PathBuf>, pid: u32) -> LockKeeper {
        LockKeeper::making(home, pid, make_inotify)
    }

    /// As `new`, with what makes the inotify instance given: the seam for a machine at its limit.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_inotify(home: Option<PathBuf>, pid: u32, init: fn() -> io::Result<OwnedFd>) -> LockKeeper {
        LockKeeper::making(home, pid, init)
    }

    fn making(home: Option<PathBuf>, pid: u32, make_inotify: fn() -> io::Result<OwnedFd>) -> LockKeeper {
        LockKeeper {
            home,
            pid,
            make_inotify,
            kept: None,
        }
    }
}

impl Housekeeping for LockKeeper {
    /// The watch goes on before the file is published, so a removal right after the publish is seen, and a watch that
    /// cannot be had leaves no file. The publish's own rename raises an event only for its temporary name.
    fn start(&mut self, port: u16, token: &str) -> Result<(), String> {
        let dir = lock::open_lock_dir(self.home.as_deref()).map_err(str::to_string)?;
        lock::sweep(&dir);
        let no_watch = |err: io::Error| format!("terminal claude: cannot watch ~/.claude/ide: {err}; not attached");
        let inotify = (self.make_inotify)().map_err(no_watch)?;
        let watch = add_watch(inotify.as_fd(), &dir).map_err(no_watch)?;
        let text = lock::lock_text(self.pid, token);
        let identity = lock::publish(&dir, port, &text)
            .map_err(|err| format!("terminal claude: cannot write the lock file: {err}; not attached"))?;
        self.kept = Some(Kept {
            port,
            name: lock::lock_name(port),
            text,
            dir,
            identity,
            inotify,
            watch: Some(watch),
            owed: false,
            last_try: None,
            said_written: false,
            said_refused: false,
        });
        Ok(())
    }

    fn watch_fd(&self) -> Option<RawFd> {
        self.kept.as_ref().map(|kept| kept.inotify.as_raw_fd())
    }

    fn on_wake(&mut self, now: Instant) {
        let home = self.home.as_deref();
        let Some(kept) = self.kept.as_mut() else {
            return;
        };
        kept.read_events();
        if kept.owed {
            kept.settle(home, now);
        }
    }

    fn retry_in(&self, now: Instant) -> Option<Duration> {
        let kept = self.kept.as_ref().filter(|kept| kept.owed)?;
        let next = kept.last_try.map_or(now, |last| last + PACE);
        Some(next.saturating_duration_since(now))
    }

    /// Closing the instance comes first, and with it goes anything owed, so nothing can write the file back after it
    /// is removed. Failing to remove it costs nothing: a dead window's lock is swept.
    fn finish(&mut self) {
        let Some(kept) = self.kept.take() else {
            return;
        };
        let Kept {
            port,
            dir,
            identity,
            inotify,
            ..
        } = kept;
        drop(inotify);
        let _ = lock::remove(&dir, port, identity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WATCH: c_int = 7;

    /// One event as the kernel lays it out, its name padded with NULs to the alignment of the next.
    fn event(wd: c_int, mask: u32, name: &str) -> Vec<u8> {
        let mut padded = name.as_bytes().to_vec();
        if !name.is_empty() {
            padded.resize((name.len() / 16 + 1) * 16, 0);
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&wd.to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&(padded.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(&padded);
        bytes
    }

    fn seen(events: &[Vec<u8>]) -> Seen {
        look_through(&events.concat(), Some(WATCH), OsStr::new("46321.lock"))
    }

    #[test]
    fn the_own_name_going_is_seen_whichever_way_it_goes() {
        for mask in [libc::IN_DELETE, libc::IN_MOVED_FROM] {
            let events = [event(WATCH, mask, "46321.lock")];
            assert!(seen(&events).file_went);
        }
    }

    #[test]
    fn another_name_is_nothing() {
        for name in ["46321.lock.bak", "4632.lock", ".46321.lock.1.2.tmp", ""] {
            assert_eq!(
                seen(&[event(WATCH, libc::IN_DELETE, name)]),
                Seen::default(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn each_sign_of_the_directory_going_is_seen() {
        for mask in [libc::IN_DELETE_SELF, libc::IN_MOVE_SELF, libc::IN_IGNORED] {
            assert!(seen(&[event(WATCH, mask, "")]).directory_went);
        }
    }

    #[test]
    fn an_event_of_a_replaced_watch_is_nothing() {
        let events = [
            event(WATCH - 1, libc::IN_DELETE, "46321.lock"),
            event(WATCH - 1, libc::IN_IGNORED, ""),
        ];
        assert_eq!(seen(&events), Seen::default());
    }

    #[test]
    fn an_overflow_says_events_were_lost_with_no_watch_at_all() {
        assert!(seen(&[event(-1, libc::IN_Q_OVERFLOW, "")]).events_lost);
        let unwatched = look_through(&event(-1, libc::IN_Q_OVERFLOW, ""), None, OsStr::new("46321.lock"));
        assert!(unwatched.events_lost);
    }

    #[test]
    fn events_in_one_read_are_all_looked_at() {
        let events = [
            event(WATCH, libc::IN_MOVED_FROM, ".46321.lock.1.2.tmp"),
            event(WATCH, libc::IN_DELETE, "46321.lock"),
            event(WATCH, libc::IN_DELETE_SELF, ""),
        ];
        let seen = seen(&events);
        assert!(seen.file_went && seen.directory_went);
    }

    #[test]
    fn a_refusal_on_a_live_window_does_not_say_it_is_not_attached() {
        let line = refused_line(lock::NOT_PRIVATE);
        assert!(line.contains("not a private directory"), "{line}");
        assert!(!line.contains("not attached"), "{line}");
    }

    #[test]
    fn a_failure_with_no_such_ending_is_kept_whole() {
        assert_eq!(
            refused_line("terminal claude: cannot write the lock file: disk full"),
            "terminal claude: cannot write the lock file: disk full; the listener stays up, trying again every second"
        );
    }

    #[test]
    fn a_cut_read_ends_the_look_without_a_panic() {
        let whole = event(WATCH, libc::IN_DELETE, "46321.lock");
        for len in 0..whole.len() {
            look_through(&whole[..len], Some(WATCH), OsStr::new("46321.lock"));
        }
    }
}
