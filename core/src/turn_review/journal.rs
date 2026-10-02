//! The revert journal: what an in-place rewrite of a project file was doing, kept on disk for as
//! long as the rewrite runs, so a crash halfway through leaves the file's earlier bytes findable.
//!
//! A file that cannot be replaced by a rename (it has another hard link, another owner, extended
//! attributes or an ACL) is rewritten in place, and a rewrite in place is not atomic. Before the
//! file is truncated, an entry `journal/<id>.json` (0600, in a 0700 directory under the review
//! directory, never in the project) names the file and the stored blobs of its bytes before and
//! after; the two blobs get refs of their own in the shadow (`refs/eitri-journal/<id>/`), which
//! retention never ages out. The entry is removed once the write, or the putting back of the
//! earlier bytes, is verified; an entry that outlives its writer is a write that never finished.
//!
//! The writer holds an `flock` on its entry for the write's whole duration, from before the
//! entry's first byte: a reader that cannot take that lock is looking at a write still running (in
//! any window), not a crash.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::shadow::{self, Shadow, ShadowError, READ_TIMEOUT};

/// The largest entry file read back; a real one is a few hundred bytes.
const MAX_ENTRY_BYTES: u64 = 64 * 1024;

/// How many times, and how far apart, [`Journal::claim`] asks for a lock it found held.
const CLAIM_ATTEMPTS: usize = 10;
const CLAIM_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// What a journal entry records about one in-place write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalNote {
    /// The provider session the write was made for.
    pub session: String,
    /// The file, relative to the project root.
    pub path: PathBuf,
    /// The stored blob of the file's bytes before the write.
    pub pre: Option<String>,
    /// The file's permission bits before the write.
    pub pre_mode: u32,
    /// The stored blob of the bytes being written.
    pub intended: Option<String>,
}

/// An entry left by a write that did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub id: String,
    /// The file, relative to the project root.
    pub path: PathBuf,
    /// The stored blob of the file's bytes before the write.
    pub pre: Option<String>,
    /// The stored blob of the bytes that were being written.
    pub intended: Option<String>,
    /// The file's permission bits before the write.
    pub mode: u32,
    /// When the write started, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The process that wrote it.
    pub pid: u32,
    /// The provider session the write was made for.
    pub session: String,
}

/// The journal directory of one project's review store.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

/// A journal entry being written for: its `flock` is held until this drops.
#[derive(Debug)]
pub(crate) struct JournalHold {
    id: String,
    _file: File,
}

impl JournalHold {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

/// An unfinished entry taken by one window to restore or forget: its `flock` is held until this
/// drops, so no other window takes the same entry meanwhile, and none lists it as pending.
#[derive(Debug)]
pub(crate) struct JournalClaim {
    _file: File,
}

#[derive(Serialize, Deserialize)]
struct EntryFile {
    v: u32,
    id: String,
    path_hex: String,
    pre: Option<String>,
    intended: Option<String>,
    mode: u32,
    session: String,
    at_ms: u64,
    pid: u32,
}

impl Journal {
    /// Opens (creating on first use) `<review_dir>/journal`, 0700. A `journal` that is a symlink,
    /// even to a directory of this user's, is refused: the entries hold the bytes of the user's
    /// files and go only where the review directory itself is.
    pub fn open(review_dir: &Path) -> Result<Journal, ShadowError> {
        let dir = review_dir.join("journal");
        let refuse = || {
            ShadowError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a directory of this user's own", dir.display()),
            ))
        };
        if dir.symlink_metadata().is_ok() && !agent::private_fs::is_own_real_dir(&dir) {
            return Err(refuse());
        }
        agent::private_fs::create_private_dir_all(&dir, shadow::private_root(review_dir))?;
        if !agent::private_fs::is_own_real_dir(&dir) {
            return Err(refuse());
        }
        Ok(Journal { dir })
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes an entry for `note` and holds it. Under the store's shared lock, held across both
    /// steps, the blobs are pinned and the entry is written, synced and moved into place; a
    /// collection, which holds that lock exclusively, therefore sees the entry and its pins or
    /// neither. The entry is locked before its first byte is written and renamed into place whole,
    /// so a reader never meets a half-written entry or an unlocked one that is still being made.
    pub(crate) fn begin(&self, note: &JournalNote, shadow: &Shadow) -> Result<JournalHold, ShadowError> {
        shadow::validate_session(&note.session)?;
        if !is_project_relative(&note.path) {
            return Err(ShadowError::InvalidPath(note.path.clone()));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let entry = EntryFile {
            v: 1,
            id: id.clone(),
            path_hex: shadow::hex_encode(note.path.as_os_str().as_bytes()),
            pre: note.pre.clone(),
            intended: note.intended.clone(),
            mode: note.pre_mode,
            session: note.session.clone(),
            at_ms: wall_clock_ms(),
            pid: std::process::id(),
        };
        let json = serde_json::to_vec(&entry).map_err(|e| ShadowError::Io(std::io::Error::other(e)))?;

        // Bounded: a collection holds the lock for as long as it runs, and a write waiting that
        // long would only leave the user wondering; it fails before the file is touched.
        let _lock = shadow.lock_shared_until(Instant::now() + READ_TIMEOUT)?;
        shadow.pin_journal_locked(&id, note.pre.as_deref(), note.intended.as_deref())?;
        let temporary = self.dir.join(format!(".{id}.json.tmp"));
        let written = (|| -> std::io::Result<File> {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(agent::private_fs::PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            flock(&file, libc::LOCK_EX)?;
            file.set_permissions(std::fs::Permissions::from_mode(agent::private_fs::PRIVATE_FILE_MODE))?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&temporary, self.dir.join(format!("{id}.json")))?;
            sync_dir(&self.dir)?;
            Ok(file)
        })();
        match written {
            Ok(file) => Ok(JournalHold { id, _file: file }),
            Err(e) => {
                let _ = std::fs::remove_file(&temporary);
                if let Err(unpin) = shadow.unpin_journal_locked(&id) {
                    eprintln!("[review] a journal entry that was never written left its pins: {unpin}");
                }
                Err(e.into())
            }
        }
    }

    /// The entries of writes that did not finish, oldest first. An entry whose writer still holds
    /// its lock (a write running now, in this window or another) is skipped. An entry that cannot be
    /// read or understood is logged and left where it is, never deleted: it may be all that is left
    /// of a file's earlier bytes.
    pub fn pending(&self) -> Vec<JournalEntry> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("[review] the revert journal could not be read: {e}");
                return Vec::new();
            }
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.as_bytes().strip_suffix(b".json") else {
                continue;
            };
            if stem.starts_with(b".") {
                continue;
            }
            let path = entry.path();
            if !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
                continue;
            }
            match read_unheld(&path) {
                Ok(None) => {}
                // Handled by another worker since the directory was listed.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Ok(Some((_unlocked_on_drop, bytes))) => match parse_entry(stem, &bytes) {
                    Some(parsed) => found.push(parsed),
                    None => eprintln!(
                        "[review] the revert journal entry {} is not one this build understands; it is kept",
                        path.display()
                    ),
                },
                Err(e) => eprintln!(
                    "[review] the revert journal entry {} could not be read: {e}; it is kept",
                    path.display()
                ),
            }
        }
        found.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.id.cmp(&b.id)));
        found
    }

    /// Takes entry `id` for this window: its lock is taken and its bytes read on one descriptor,
    /// held until the claim drops. `None` when there is no such entry, or when another holds it (a
    /// write still running, or another window restoring it).
    ///
    /// A lock found held is asked again for a moment before giving up: a child process another
    /// thread is starting shares every open descriptor of this process until it execs, a lock of
    /// this window's own included, so a lock just let go can look held for that long.
    pub(crate) fn claim(&self, id: &str) -> Result<Option<(JournalEntry, JournalClaim)>, ShadowError> {
        if !shadow::is_journal_id(id) {
            return Err(ShadowError::InvalidName(id.to_owned()));
        }
        let path = self.dir.join(format!("{id}.json"));
        let mut attempts = CLAIM_ATTEMPTS;
        let (file, bytes) = loop {
            match read_unheld(&path) {
                Ok(Some(read)) => break read,
                Ok(None) if attempts > 1 => {
                    attempts -= 1;
                    std::thread::sleep(CLAIM_RETRY);
                }
                Ok(None) => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        };
        match parse_entry(id.as_bytes(), &bytes) {
            Some(entry) => Ok(Some((entry, JournalClaim { _file: file }))),
            None => Err(ShadowError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the revert journal entry {id} is not one this build understands"),
            ))),
        }
    }

    /// Removes entry `id`: the entry first (and the directory synced), then its pins. A crash in
    /// between leaves pins with no entry, which the next collection removes; failing to unpin is
    /// logged and is not an error, since the entry, which is what says a write is unfinished, is gone.
    pub fn remove(&self, id: &str, shadow: &Shadow) -> Result<(), ShadowError> {
        if !shadow::is_journal_id(id) {
            return Err(ShadowError::InvalidName(id.to_owned()));
        }
        match std::fs::remove_file(self.dir.join(format!("{id}.json"))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        sync_dir(&self.dir)?;
        if let Err(e) = shadow.unpin_journal(id) {
            eprintln!("[review] the pins of journal entry {id} could not be removed: {e}");
        }
        Ok(())
    }
}

/// `path`'s bytes when nobody holds its lock, and the descriptor that now holds it; `None` while
/// another does. The lock is taken and the bytes read on one descriptor, opened without following
/// a link or blocking, of a regular file this user owns.
fn read_unheld(path: &Path) -> std::io::Result<Option<(File, Vec<u8>)>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    lock_and_read(file, path)
}

/// The second half of [`read_unheld`], on a descriptor already open. Once the lock is held, `path`
/// must still name the file locked: an entry restored or forgotten by another worker is unlinked
/// and then let go, and a descriptor opened before the unlink would otherwise take the lock of a
/// file nobody can find any more and hand back an entry already handled. That case is `NotFound`.
fn lock_and_read(mut file: File, path: &Path) -> std::io::Result<Option<(File, Vec<u8>)>> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a regular file of this user's",
        ));
    }
    match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => return Ok(None),
        Err(e) => return Err(e),
    }
    let gone = || std::io::Error::new(std::io::ErrorKind::NotFound, "the entry was removed meanwhile");
    match path.symlink_metadata() {
        Ok(now) if now.dev() == meta.dev() && now.ino() == meta.ino() => {}
        Ok(_) => return Err(gone()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(gone()),
        Err(e) => return Err(e),
    }
    if meta.len() > MAX_ENTRY_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "too large"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(Some((file, bytes)))
}

/// An entry read back, when it is a version-1 entry named by its own id whose path stays inside the
/// project and whose session can name a ref.
fn parse_entry(stem: &[u8], bytes: &[u8]) -> Option<JournalEntry> {
    let entry: EntryFile = serde_json::from_slice(bytes).ok()?;
    if entry.v != 1 || !shadow::is_journal_id(&entry.id) || entry.id.as_bytes() != stem {
        return None;
    }
    shadow::validate_session(&entry.session).ok()?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(shadow::hex_decode(&entry.path_hex)?));
    if !is_project_relative(&path) {
        return None;
    }
    Some(JournalEntry {
        id: entry.id,
        path,
        pre: entry.pre,
        intended: entry.intended,
        mode: entry.mode,
        at_ms: entry.at_ms,
        pid: entry.pid,
        session: entry.session,
    })
}

/// Relative, not empty, and made only of names: no root, no `.`, no `..`.
fn is_project_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.as_os_str().as_bytes().contains(&0)
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    loop {
        // SAFETY: the descriptor is `file`'s own and open for the call; `flock` takes no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

/// Makes the directory's entries durable: a renamed-in or removed entry survives a crash.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?
        .sync_all()
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_outside_the_project_or_misnamed_does_not_parse() {
        let id = "0123456789abcdef0123456789abcdef";
        let entry = |path: &[u8], id: &str| {
            serde_json::to_vec(&EntryFile {
                v: 1,
                id: id.into(),
                path_hex: shadow::hex_encode(path),
                pre: None,
                intended: None,
                mode: 0o644,
                session: "s".into(),
                at_ms: 1,
                pid: 1,
            })
            .unwrap()
        };
        assert!(parse_entry(id.as_bytes(), &entry(b"src/x.rs", id)).is_some());
        for bad in [&b"/etc/passwd"[..], b"../x", b"a/../b", b"./a"] {
            assert!(parse_entry(id.as_bytes(), &entry(bad, id)).is_none(), "{bad:?}");
        }
        let mut bad_session: EntryFile = serde_json::from_slice(&entry(b"x", id)).unwrap();
        bad_session.session = "../s".into();
        assert!(parse_entry(id.as_bytes(), &serde_json::to_vec(&bad_session).unwrap()).is_none());
        let other = "fedcba9876543210fedcba9876543210";
        assert!(parse_entry(other.as_bytes(), &entry(b"x", id)).is_none());
        assert!(parse_entry(b"x", b"{not json").is_none());
    }

    #[test]
    fn an_entry_unlinked_or_replaced_before_the_lock_is_not_handed_back() {
        let dir = crate::test_scratch_dir::ScratchDir::new("eitri-journal", "relinked");
        let path = dir.join("entry.json");
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
                .unwrap()
        };

        std::fs::write(&path, b"one").unwrap();
        let (_, bytes) = lock_and_read(open(), &path).unwrap().expect("unheld");
        assert_eq!(bytes, b"one");

        // Opened, then removed by another worker before this one locks it.
        let early = open();
        std::fs::remove_file(&path).unwrap();
        let err = lock_and_read(early, &path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

        // Opened, then the name given to another file.
        std::fs::write(&path, b"old").unwrap();
        let early = open();
        let other = dir.join("other.json");
        std::fs::write(&other, b"new").unwrap();
        std::fs::rename(&other, &path).unwrap();
        let err = lock_and_read(early, &path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
