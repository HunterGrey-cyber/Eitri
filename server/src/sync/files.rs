//! Reading one project file, replacing or removing it only while it is what the laptop last read, and waiting for it
//! to reach a given hash.
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::Instant;

use eitri_core::turn_review::write::{compare_and_remove, compare_and_replace, ProjectDir, WriteError};
use eitri_server_proto::{
    ReadFileChunk, RemoveFileIfRequest, RemoveFileIfResponse, ReplaceFileIfRequest, ReplaceFileIfResponse,
    WaitFileHashRequest, WaitFileHashResponse,
};
use sha2::{Digest, Sha256};

use super::listing::{lstat_at, make_parents, open_file_at, reach, read_link_at, Kind, Reached};
use super::{audit_word, hex, parse_hash, Root, CHUNK_BYTES, MAX_READ_BYTES, MAX_REPLACE_BYTES, MAX_WAIT_MS};

/// What a path is now, as a laptop is told it: a regular file by its hash, anything else by its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    File([u8; 32]),
    Missing,
    Directory,
    Symlink(Vec<u8>),
    Other,
    /// A regular file larger than [`MAX_READ_BYTES`]: it is not hashed.
    TooLarge,
    /// A regular file that held more bytes than its size said when it was opened: it is being written.
    Growing,
    /// A regular file whose hash was not finished by the call's deadline.
    Unfinished,
}

impl Fact {
    /// The wire's word for it: a file's hash in hex, or `missing`, `directory`, `symlink`, `other`, `too-large`,
    /// `growing`, `unfinished`. A hash is never one of the words, so the two cannot be mistaken.
    pub fn observed(&self) -> String {
        match self {
            Fact::File(digest) => hex(digest),
            Fact::Missing => "missing".into(),
            Fact::Directory => "directory".into(),
            Fact::Symlink(_) => "symlink".into(),
            Fact::Other => "other".into(),
            Fact::TooLarge => "too-large".into(),
            Fact::Growing => "growing".into(),
            Fact::Unfinished => "unfinished".into(),
        }
    }
}

/// What `rel` is now. A regular file is hashed through a descriptor opened without following a link, only up to the
/// size it had when opened and never past [`MAX_READ_BYTES`], with `deadline` looked at between chunks.
pub fn fact(root: &Root, rel: &str, deadline: Instant) -> Result<Fact, String> {
    let (parent, leaf) = match reach(&root.dir, rel)? {
        Reached::MissingParent => return Ok(Fact::Missing),
        Reached::Parent(parent, leaf) => (parent, leaf),
    };
    let st = match lstat_at(parent.as_raw_fd(), &leaf) {
        Ok(st) => st,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(Fact::Missing),
        Err(error) => return Err(format!("{rel}: {error}")),
    };
    match Kind::of(st.st_mode) {
        Kind::Directory => Ok(Fact::Directory),
        Kind::Symlink => match read_link_at(parent.as_raw_fd(), &leaf) {
            Ok(target) => Ok(Fact::Symlink(target)),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(Fact::Missing),
            Err(error) => Err(format!("{rel}: {error}")),
        },
        Kind::File => {
            let (file, fst) = match open_file_at(parent.as_raw_fd(), &leaf) {
                Ok(opened) => opened,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(Fact::Missing),
                Err(error) => return Err(format!("{rel}: {error}")),
            };
            if Kind::of(fst.st_mode) != Kind::File {
                return Ok(Fact::Other);
            }
            let size = fst.st_size as u64;
            if size > MAX_READ_BYTES {
                return Ok(Fact::TooLarge);
            }
            let mut hasher = Sha256::new();
            let mut chunk = vec![0u8; CHUNK_BYTES];
            let mut total = 0u64;
            let mut reader = file.take(size + 1);
            loop {
                if Instant::now() >= deadline {
                    return Ok(Fact::Unfinished);
                }
                match reader.read(&mut chunk) {
                    Ok(0) => return Ok(Fact::File(hasher.finalize().into())),
                    Ok(read) => {
                        total += read as u64;
                        if total > size {
                            return Ok(Fact::Growing);
                        }
                        hasher.update(&chunk[..read]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(format!("{rel}: {error}")),
                }
            }
        }
        Kind::Other | Kind::NotUtf8 => Ok(Fact::Other),
    }
}

/// Streams `rel` through `emit` in data chunks of at most [`CHUNK_BYTES`] and returns the last chunk: the kind
/// (`file`, `missing`, `directory`, `symlink` with its target as the data, `other`) and, for a file, its SHA-256. A
/// file larger than `limit`, or one that grows past it while it is read, gives a refusal naming the limit instead.
/// `None` when `emit` answered `false` (nobody reads any more, or the call ran out of time) and the read stopped.
pub fn read_file(
    root: &Root,
    rel: &str,
    limit: u64,
    deadline: Instant,
    emit: &mut dyn FnMut(ReadFileChunk) -> bool,
) -> Option<ReadFileChunk> {
    let result = read_into(root, rel, limit, deadline, emit);
    if Instant::now() >= deadline {
        return Some(ReadFileChunk {
            refusal: format!("the read took longer than {} s", super::READ_DEADLINE.as_secs()),
            ..Default::default()
        });
    }
    match result {
        Ok(last) => last,
        Err(refusal) => Some(ReadFileChunk {
            refusal,
            ..Default::default()
        }),
    }
}

fn read_into(
    root: &Root,
    rel: &str,
    limit: u64,
    deadline: Instant,
    emit: &mut dyn FnMut(ReadFileChunk) -> bool,
) -> Result<Option<ReadFileChunk>, String> {
    if Instant::now() >= deadline {
        return Ok(None);
    }
    super::rel(rel)?;
    if limit > MAX_READ_BYTES {
        return Err(format!(
            "a read is limited to {MAX_READ_BYTES} bytes; {limit} was asked"
        ));
    }
    let kind = |kind: &str| {
        Ok(Some(ReadFileChunk {
            kind: kind.into(),
            ..Default::default()
        }))
    };
    let (parent, leaf) = match reach(&root.dir, rel)? {
        Reached::MissingParent => return kind("missing"),
        Reached::Parent(parent, leaf) => (parent, leaf),
    };
    let st = match lstat_at(parent.as_raw_fd(), &leaf) {
        Ok(st) => st,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return kind("missing"),
        Err(error) => return Err(format!("{rel}: {error}")),
    };
    match Kind::of(st.st_mode) {
        Kind::Directory => return kind("directory"),
        Kind::Symlink => {
            return match read_link_at(parent.as_raw_fd(), &leaf) {
                Ok(target) => Ok(Some(ReadFileChunk {
                    data: target,
                    kind: "symlink".into(),
                    ..Default::default()
                })),
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => kind("missing"),
                Err(error) => Err(format!("{rel}: {error}")),
            }
        }
        Kind::Other | Kind::NotUtf8 => return kind("other"),
        Kind::File => {}
    }
    let over = |size: u64| format!("{rel} is {size} bytes, more than the {limit}-byte limit of this read");
    if st.st_size as u64 > limit {
        return Err(over(st.st_size as u64));
    }
    let (file, fst) = match open_file_at(parent.as_raw_fd(), &leaf) {
        Ok(opened) => opened,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return kind("missing"),
        Err(error) => return Err(format!("{rel}: {error}")),
    };
    if (fst.st_dev, fst.st_ino) != (st.st_dev, st.st_ino) || Kind::of(fst.st_mode) != Kind::File {
        return Err(format!("{rel} changed while it was being opened"));
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    // One byte past the limit is enough to know the file grew beyond it.
    let mut reader = file.take(limit.saturating_add(1));
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("{rel}: {error}")),
        };
        total += read as u64;
        if total > limit {
            return Err(over(total));
        }
        hasher.update(&chunk[..read]);
        if !emit(ReadFileChunk {
            data: chunk[..read].to_vec(),
            ..Default::default()
        }) {
            return Ok(None);
        }
    }
    Ok(Some(ReadFileChunk {
        sha256: hex(&hasher.finalize().into()),
        kind: "file".into(),
        ..Default::default()
    }))
}

/// The refusal for a path that is no longer what the laptop read, with what it is now.
fn changed(root: &Root, rel: &str) -> String {
    let now = match fact(root, rel, Instant::now() + super::FACT_DEADLINE) {
        Ok(fact) => fact.observed(),
        Err(error) => format!("unreadable: {error}"),
    };
    format!("{rel} changed on the server since it was read (now: {now})")
}

/// The write side of the held root: the same directory, from a copy of its descriptor, never its path again.
fn project(root: &Root) -> Result<ProjectDir, String> {
    root.dir
        .try_clone()
        .and_then(|dir| ProjectDir::from_fd(OwnedFd::from(dir)))
        .map_err(|error| format!("the root: {error}"))
}

/// Replaces `rel` with the request's bytes only while it is exactly what was read: a file with `expected_sha256`, or
/// nothing at all when that is empty (missing directories on the way are then made). Audited as `replace`.
pub fn replace_if(state: &Path, root: &Root, request: &ReplaceFileIfRequest) -> ReplaceFileIfResponse {
    let outcome = (|| {
        super::rel(&request.rel)?;
        if request.data.len() > MAX_REPLACE_BYTES {
            return Err(format!(
                "a replacement is limited to {MAX_REPLACE_BYTES} bytes; {} were sent",
                request.data.len()
            ));
        }
        let expected = match request.expected_sha256.as_str() {
            "" => None,
            text => Some(parse_hash(text)?),
        };
        if expected.is_none() {
            make_parents(&root.dir, &request.rel)?;
        }
        let target = project(root)?
            .target(Path::new(&request.rel), false)
            .map_err(|error| error.to_string())?;
        match compare_and_replace(&target, expected, &request.data) {
            Ok(()) => Ok(()),
            Err(WriteError::Changed | WriteError::Appeared) => Err(changed(root, &request.rel)),
            Err(error) => Err(error.to_string()),
        }
    })();
    audit(state, "replace", root, &request.rel, &outcome);
    match outcome {
        Ok(()) => ReplaceFileIfResponse {
            replaced: true,
            refusal: String::new(),
        },
        Err(refusal) => ReplaceFileIfResponse {
            replaced: false,
            refusal,
        },
    }
}

/// Removes `rel` only while it is a regular file with `expected_sha256`; a directory, a link or other bytes are refused
/// with what is there now. Audited as `remove`.
pub fn remove_if(state: &Path, root: &Root, request: &RemoveFileIfRequest) -> RemoveFileIfResponse {
    let outcome = (|| {
        super::rel(&request.rel)?;
        let expected = parse_hash(&request.expected_sha256)?;
        let target = project(root)?
            .target(Path::new(&request.rel), false)
            .map_err(|error| error.to_string())?;
        match compare_and_remove(&target, expected) {
            Ok(()) => Ok(()),
            Err(WriteError::Changed) => Err(changed(root, &request.rel)),
            Err(error) => Err(error.to_string()),
        }
    })();
    audit(state, "remove", root, &request.rel, &outcome);
    match outcome {
        Ok(()) => RemoveFileIfResponse {
            removed: true,
            refusal: String::new(),
        },
        Err(refusal) => RemoveFileIfResponse {
            removed: false,
            refusal,
        },
    }
}

fn audit(state: &Path, event: &str, root: &Root, rel: &str, outcome: &Result<(), String>) {
    let result = match outcome {
        Ok(()) => "done".to_owned(),
        Err(refusal) => format!("refused {}", audit_word(refusal)),
    };
    let fields = format!("root={} rel={} {result}", audit_word(&root.canonical), audit_word(rel));
    let _ = crate::audit::record(state, event, &fields);
}

/// Waits until `rel` holds the request's hash (or is missing, when the hash is empty), at most `timeout_ms`. It looks
/// once after its watch is armed, so a write that landed before the call is seen, and then only when the file system
/// says something happened at the path: a write closed, a name moved into place or created (on the nearest directory
/// that exists, following new directories down to the file), or the file removed. Returns whether it matched and the
/// last thing it saw (see [`Fact::observed`]).
pub fn wait_hash(root: &Root, request: &WaitFileHashRequest) -> WaitFileHashResponse {
    let refused = |refusal: String| WaitFileHashResponse {
        refusal,
        ..Default::default()
    };
    if let Err(refusal) = super::rel(&request.rel) {
        return refused(refusal);
    }
    if request.timeout_ms > MAX_WAIT_MS {
        return refused(format!(
            "a wait is limited to {MAX_WAIT_MS} ms; {} was asked",
            request.timeout_ms
        ));
    }
    let wanted = match request.sha256.as_str() {
        "" => Fact::Missing,
        text => match parse_hash(text) {
            Ok(digest) => Fact::File(digest),
            Err(refusal) => return refused(refusal),
        },
    };
    match watch::wait(
        root,
        &request.rel,
        &wanted,
        std::time::Duration::from_millis(request.timeout_ms),
    ) {
        Ok((matched, seen)) => WaitFileHashResponse {
            matched,
            observed: seen.observed(),
            refusal: String::new(),
        },
        Err(refusal) => refused(refusal),
    }
}

#[cfg(target_os = "linux")]
mod watch {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    use super::super::listing::{c_name, open_dir_at};
    use super::{fact, Fact, Root};

    /// Where the wait's one watch is: the deepest directory on the path that exists, and the name in it that leads on
    /// (the file itself when the directory is its parent).
    struct Armed {
        wd: i32,
        next: CString,
    }

    const PARENT_EVENTS: u32 = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;
    /// How long the last look after a wait's deadline may spend hashing.
    const LAST_LOOK: Duration = Duration::from_secs(1);
    const ANCESTOR_EVENTS: u32 = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

    pub(super) fn wait(root: &Root, rel: &str, wanted: &Fact, timeout: Duration) -> Result<(bool, Fact), String> {
        let deadline = Instant::now() + timeout;
        // SAFETY: inotify_init1 takes no pointers; a new descriptor that nothing else owns is returned.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(format!("cannot watch {rel}: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a fresh descriptor owned only here.
        let inotify = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut armed = arm(&inotify, root, rel, None)?;
        let mut seen = fact(root, rel, deadline)?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if &seen == wanted {
                return Ok((true, seen));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok((false, seen));
            }
            let mut poll = libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = left.as_millis().clamp(1, i32::MAX as u128) as i32;
            // SAFETY: `poll` is one valid pollfd for the call.
            let ready = unsafe { libc::poll(&mut poll, 1, millis) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(format!("cannot watch {rel}: {error}"));
            }
            if ready == 0 {
                // The deadline passed with nothing at the path; a last look, given a moment to hash, reports what is there.
                seen = fact(root, rel, Instant::now() + LAST_LOOK)?;
                return Ok((&seen == wanted, seen));
            }
            let mut relevant = false;
            loop {
                // SAFETY: `buffer` is valid and writable for its length; the descriptor is open.
                let read = unsafe { libc::read(inotify.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EAGAIN) => break,
                        Some(libc::EINTR) => continue,
                        _ => return Err(format!("cannot watch {rel}: {error}")),
                    }
                }
                relevant |= concerns(&buffer[..read as usize], &armed);
            }
            if relevant {
                armed = arm(&inotify, root, rel, Some(armed))?;
                seen = fact(root, rel, deadline)?;
            }
        }
    }

    /// Whether any event in `events` is about the name the watch leads on, or about the watched directory itself.
    fn concerns(events: &[u8], armed: &Armed) -> bool {
        let header = std::mem::size_of::<libc::inotify_event>();
        let mut offset = 0;
        let mut relevant = false;
        while offset + header <= events.len() {
            // SAFETY: the kernel wrote whole events; the header is read unaligned from inside the buffer.
            let event: libc::inotify_event = unsafe { std::ptr::read_unaligned(events[offset..].as_ptr().cast()) };
            let name_start = offset + header;
            let name_end = (name_start + event.len as usize).min(events.len());
            let name = &events[name_start..name_end];
            let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
            if event.wd == armed.wd {
                let about_self = event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0;
                relevant |= about_self || name == armed.next.as_bytes();
            }
            if event.mask & libc::IN_Q_OVERFLOW != 0 {
                relevant = true;
            }
            offset = name_end;
        }
        relevant
    }

    /// Watches the deepest directory on `rel`'s way that exists now; the previous watch, if it is elsewhere, is
    /// removed. A link or a file on the way refuses, as reading the path would. A directory made on the way after the
    /// walk but before the watch would send the watch nothing, so once armed it looks for the next one again and moves
    /// deeper when it is there.
    fn arm(inotify: &OwnedFd, root: &Root, rel: &str, mut previous: Option<Armed>) -> Result<Armed, String> {
        let parts: Vec<&str> = rel.split('/').collect();
        loop {
            let mut dir = root
                .dir
                .try_clone()
                .map(OwnedFd::from)
                .map_err(|e| format!("{rel}: {e}"))?;
            let mut index = 0;
            while index + 1 < parts.len() {
                let name = c_name(parts[index].as_bytes()).map_err(|e| format!("{rel}: {e}"))?;
                match open_dir_at(dir.as_raw_fd(), &name) {
                    Ok(next) => dir = next,
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => break,
                    Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                        return Err(format!(
                            "{rel} is not inside the project: {} is a link or not a directory",
                            parts[index]
                        ))
                    }
                    Err(error) => return Err(format!("{rel}: {error}")),
                }
                index += 1;
            }
            let parent = index + 1 == parts.len();
            let next = c_name(parts[index].as_bytes()).map_err(|e| format!("{rel}: {e}"))?;
            // The descriptor's own entry under /proc names the very directory it holds, wherever that is now.
            let path = c_name(format!("/proc/self/fd/{}", dir.as_raw_fd()).as_bytes()).map_err(|e| e.to_string())?;
            let mask = libc::IN_ONLYDIR | if parent { PARENT_EVENTS } else { ANCESTOR_EVENTS };
            // SAFETY: `path` is NUL-terminated and outlives the call; the inotify descriptor is open.
            let wd = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), mask) };
            if wd < 0 {
                return Err(format!("cannot watch {rel}: {}", std::io::Error::last_os_error()));
            }
            if let Some(previous) = previous.take().filter(|previous| previous.wd != wd) {
                // SAFETY: removing a watch takes no pointers; a watch already gone is an error that changes nothing.
                unsafe { libc::inotify_rm_watch(inotify.as_raw_fd(), previous.wd) };
            }
            let armed = Armed { wd, next };
            if parent || open_dir_at(dir.as_raw_fd(), &armed.next).is_err() {
                return Ok(armed);
            }
            previous = Some(armed);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod watch {
    use super::{Fact, Root};

    pub(super) fn wait(
        _root: &Root,
        rel: &str,
        _wanted: &Fact,
        _timeout: std::time::Duration,
    ) -> Result<(bool, Fact), String> {
        Err(format!(
            "cannot watch {rel}: waiting for a file needs inotify, which only Linux has"
        ))
    }
}
