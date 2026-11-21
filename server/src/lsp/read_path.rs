//! `ReadPath`: the bytes of one regular file at an absolute path the helper's account can read, for the laptop's
//! `eitri-remote://` buffers (a dependency's source, the standard library, a generated file: what sync does not carry).
//!
//! It reads what `cat` would: links on the way to the file are followed, and only the last component is opened
//! without following (`O_NOFOLLOW`), so a link there is refused rather than read through. The last component is
//! opened `O_PATH` and judged on that descriptor before anything opens it to read, so a FIFO's writer or a device's
//! driver never sees the request; the read is through the same inode, and the descriptor read from is judged again (a
//! regular file, not larger than the cap), never a second look at the path. The read is opened without waiting
//! (`O_NONBLOCK`). Data goes in chunks of
//! [`CHUNK_BYTES`]; the last chunk carries the SHA-256 of everything sent, and a refusal (one line) replaces it. A
//! refusal may follow data (the file grew past the cap while it was read) and voids what came before.
//!
//! Reads change nothing and are not audited.

use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use eitri_server_proto::{ReadPathChunk, LSP_PATH_MAX, READ_PATH_MAX};
use sha2::{Digest, Sha256};

use super::super::sidecars::in_normal_form;
use super::super::sync::hex;

/// The size of each data chunk.
pub const CHUNK_BYTES: usize = 64 * 1024;

fn refusal(text: String) -> ReadPathChunk {
    ReadPathChunk {
        refusal: text,
        ..Default::default()
    }
}

/// The most bytes a request may be answered with: `limit`, at most [`READ_PATH_MAX`], and 0 for that maximum.
pub fn cap_of(limit: u64) -> u64 {
    let most = READ_PATH_MAX as u64;
    if limit == 0 {
        most
    } else {
        limit.min(most)
    }
}

/// Reads `path` and gives each data chunk to `emit` (which says whether anyone still reads); gives the chunk that ends
/// the answer: the hash, or the refusal. `None` when `emit` said nobody reads any more, or the deadline passed.
pub fn read_path(
    path: &str,
    limit: u64,
    deadline: Instant,
    emit: &mut dyn FnMut(ReadPathChunk) -> bool,
) -> Option<ReadPathChunk> {
    match read_into(path, limit, deadline, emit) {
        Ok(last) => last,
        Err(why) => Some(refusal(why)),
    }
}

/// An absolute path in normal form that is UTF-8 (it came in a string), holds no NUL and fits `LSP_PATH_MAX`.
fn checked(path: &str) -> Result<CString, String> {
    if path.len() > LSP_PATH_MAX {
        return Err(format!("the path is longer than {LSP_PATH_MAX} bytes"));
    }
    if !path.starts_with('/') || !in_normal_form(path.as_bytes()) {
        return Err(format!(
            "{path:?}: the path must be absolute, with no `.` or `..` component, no doubled or trailing slash"
        ));
    }
    CString::new(path).map_err(|_| format!("{path:?}: the path holds a NUL"))
}

fn open_error(path: &str, error: std::io::Error) -> String {
    let words = match error.raw_os_error() {
        Some(libc::ENOENT) => "does not exist".to_owned(),
        Some(libc::ENOTDIR) => "has a component that is not a directory".to_owned(),
        Some(libc::EACCES) => "cannot be read: permission denied".to_owned(),
        _ => format!("cannot be read: {error}"),
    };
    format!("{path:?} {words}")
}

/// Opens `path` to read once it is known to be a regular file. The last component is opened `O_PATH | O_NOFOLLOW`
/// first, which a FIFO's writer or a device's driver never notices, and judged on that descriptor; the file is then
/// opened to read through `/proc/self/fd`, which reopens the very inode judged. Where `/proc` is not mounted it is
/// opened by its path again (`O_NOFOLLOW | O_NONBLOCK`), and the caller's check of the descriptor read from stays.
fn opened(path: &str, c_path: &CString) -> Result<std::fs::File, String> {
    // SAFETY: `c_path` is a NUL-terminated string that lives through the call; the flags ask for no creation.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(open_error(path, std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` is the descriptor `open` just returned and nothing else owns it.
    let held = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let kind = held
        .metadata()
        .map_err(|error| format!("{path:?} cannot be read: {error}"))?
        .file_type();
    if kind.is_symlink() {
        return Err(format!("{path:?} is a symbolic link; ask for the file it points to"));
    }
    if kind.is_dir() {
        return Err(format!("{path:?} is a directory"));
    }
    if !kind.is_file() {
        return Err(format!("{path:?} is not a regular file"));
    }
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    let through_proc = CString::new(format!("/proc/self/fd/{}", held.as_raw_fd())).expect("no NUL in a number");
    // SAFETY: as above. `O_NOFOLLOW` does not apply to the `/proc` entry, which reopens the inode `held` names.
    let fd = unsafe { libc::open(through_proc.as_ptr(), flags & !libc::O_NOFOLLOW) };
    let fd = if fd >= 0 {
        fd
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(open_error(path, error));
        }
        // SAFETY: as above.
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            return Err(open_error(path, std::io::Error::last_os_error()));
        }
        fd
    };
    // SAFETY: `fd` is the descriptor `open` just returned and nothing else owns it.
    Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn read_into(
    path: &str,
    limit: u64,
    deadline: Instant,
    emit: &mut dyn FnMut(ReadPathChunk) -> bool,
) -> Result<Option<ReadPathChunk>, String> {
    if Instant::now() >= deadline {
        return Ok(None);
    }
    let cap = cap_of(limit);
    let c_path = checked(path)?;
    let file = opened(path, &c_path)?;
    let meta = file
        .metadata()
        .map_err(|error| format!("{path:?} cannot be read: {error}"))?;
    let kind = meta.file_type();
    if !kind.is_file() {
        let what = if kind.is_dir() {
            "is a directory"
        } else {
            "is not a regular file"
        };
        return Err(format!("{path:?} {what}"));
    }
    let over = |size: u64| format!("{path:?} is {size} bytes, more than the {cap}-byte limit of this read");
    if meta.len() > cap {
        return Err(over(meta.len()));
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    // One byte past the cap is enough to know the file grew beyond it while it was read.
    let mut reader = file.take(cap.saturating_add(1));
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("{path:?} cannot be read: {error}")),
        };
        total += read as u64;
        if total > cap {
            return Err(over(total));
        }
        hasher.update(&chunk[..read]);
        if !emit(ReadPathChunk {
            data: chunk[..read].to_vec(),
            ..Default::default()
        }) {
            return Ok(None);
        }
    }
    Ok(Some(ReadPathChunk {
        sha256: hex(&hasher.finalize().into()),
        ..Default::default()
    }))
}
