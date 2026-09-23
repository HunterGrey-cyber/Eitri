//! Keeps the host process's stdin away from the embedded nvim.
//!
//! **What the fork does.** The Neovide fork hands its own process's stdin to the `nvim --embed`
//! child whenever that stdin is a regular file, a FIFO or a socket: `NeovimInstance::forward_stdin`
//! (`src/bridge/session.rs` at the pinned rev `5997cef`, upstream Neovide's behaviour, so that
//! `cmd | neovide` opens `cmd`'s output) dups it, and `ui_attach` passes it as `stdin_fd`. nvim reads
//! it as a buffer and, for a pipe or a socket whose writer stays open, blocks in `read()` until EOF.
//! The editor stays blank (no theme, a default-size grid) and the window's close then times out
//! waiting for nvim. neovibe has no "read stdin into the editor" feature; being started with a pipe
//! on stdin -- by a launcher, a service, a test harness -- is ordinary. The GUI pass of 2026-09-23
//! lost 18 launches to it on both builds before it was found (its defect 2).
//!
//! **What this does instead.** The fork is not changed: it reads fd 0 when the harness starts, so
//! [`detach_stdin_from_nvim`] points fd 0 at `/dev/null` before that, and only when the fork would
//! have forwarded it ([`forwarded_kind`] restates the fork's rule). A terminal and `/dev/null` are
//! character devices and are left alone, so running neovibe from a shell changes nothing. A closed
//! fd 0 cannot reach `main`: Rust's runtime opens `/dev/null` over a closed standard fd before
//! `main` runs, and an fd `fstat` refuses is one the fork forwards nothing from either.

use std::io;
use std::os::fd::{AsRawFd, RawFd};

/// What stdin was when [`detach_stdin_from_nvim`] replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardedStdin {
    RegularFile,
    Fifo,
    Socket,
}

impl std::fmt::Display for ForwardedStdin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ForwardedStdin::RegularFile => "regular file",
            ForwardedStdin::Fifo => "pipe",
            ForwardedStdin::Socket => "socket",
        })
    }
}

/// Whether the fork forwards an fd whose `st_mode` is `mode` to nvim, and as what. The fork's own
/// `match FileType::from_raw_mode(stat.st_mode)`: a regular file, a FIFO or a socket, nothing else.
pub fn forwarded_kind(mode: libc::mode_t) -> Option<ForwardedStdin> {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Some(ForwardedStdin::RegularFile),
        libc::S_IFIFO => Some(ForwardedStdin::Fifo),
        libc::S_IFSOCK => Some(ForwardedStdin::Socket),
        _ => None,
    }
}

/// `st_mode` of `fd`, or `None` if `fstat` refuses it (a closed fd).
fn mode_of(fd: RawFd) -> Option<libc::mode_t> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid, writable `struct stat`; `fstat` on a closed or invalid fd fails
    // with EBADF and writes nothing we read.
    (unsafe { libc::fstat(fd, &mut stat) } == 0).then_some(stat.st_mode)
}

/// Points `fd` at `/dev/null` if the fork would forward it. `Ok(None)`: left alone.
fn detach(fd: RawFd) -> io::Result<Option<ForwardedStdin>> {
    let Some(kind) = mode_of(fd).and_then(forwarded_kind) else {
        return Ok(None);
    };
    let null = std::fs::File::open("/dev/null")?;
    // SAFETY: both are open fds. `dup2` closes `fd`'s old description and makes `fd` refer to
    // `/dev/null`, without `FD_CLOEXEC`, as a stdin should be; `null` itself is closed on drop.
    if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(kind))
}

/// Points this process's stdin at `/dev/null` if the Neovide fork would otherwise hand it to the
/// embedded nvim (module doc). Returns what it replaced, or `None` if stdin was left alone.
///
/// **A host calls this once, before the first [`crate::NeovideEditorPane`] starts nvim** -- `shell`
/// does it first thing in `main`, before any thread exists. It is not done inside the pane: it
/// changes the whole process's stdin, which is the host's to decide, and a pane constructed after
/// something else read stdin would be too late to help anyway.
pub fn detach_stdin_from_nvim() -> io::Result<Option<ForwardedStdin>> {
    detach(libc::STDIN_FILENO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe() -> (RawFd, RawFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is two writable ints.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn close(fd: RawFd) {
        // SAFETY: an fd this test opened and owns.
        unsafe { libc::close(fd) };
    }

    fn is_char_device(fd: RawFd) -> bool {
        mode_of(fd).is_some_and(|mode| mode & libc::S_IFMT == libc::S_IFCHR)
    }

    /// The fork's rule, type by type: what it forwards and what it does not.
    #[test]
    fn the_fork_forwards_a_regular_file_a_fifo_and_a_socket_and_nothing_else() {
        assert_eq!(forwarded_kind(libc::S_IFREG | 0o644), Some(ForwardedStdin::RegularFile));
        assert_eq!(forwarded_kind(libc::S_IFIFO | 0o600), Some(ForwardedStdin::Fifo));
        assert_eq!(forwarded_kind(libc::S_IFSOCK | 0o777), Some(ForwardedStdin::Socket));
        for (not, what) in [
            (libc::S_IFCHR, "a terminal or /dev/null"),
            (libc::S_IFDIR, "a directory"),
            (libc::S_IFBLK, "a block device"),
        ] {
            assert_eq!(forwarded_kind(not | 0o666), None, "{what}");
        }
    }

    /// The GUI pass's case: a pipe whose writer is still open, which nvim would have blocked on.
    /// Afterwards the fd is `/dev/null`, and a read returns EOF at once instead of waiting.
    #[test]
    fn a_pipe_with_its_writer_still_open_is_replaced_by_dev_null() {
        let (read_end, write_end) = pipe();
        assert_eq!(detach(read_end).unwrap(), Some(ForwardedStdin::Fifo));
        // Checked before the read: were it still the pipe, the read would block forever.
        assert!(is_char_device(read_end), "now /dev/null");
        let mut byte = [0u8; 1];
        // SAFETY: `byte` is one writable byte; `read_end` is open.
        assert_eq!(unsafe { libc::read(read_end, byte.as_mut_ptr().cast(), 1) }, 0, "EOF");
        close(read_end);
        close(write_end);
    }

    /// The pass reproduced it with a socketpair; a socket is replaced the same way.
    #[test]
    fn a_socket_is_replaced_by_dev_null() {
        let mut fds = [0; 2];
        // SAFETY: `fds` is two writable ints.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        assert_eq!(detach(fds[0]).unwrap(), Some(ForwardedStdin::Socket));
        assert!(is_char_device(fds[0]));
        close(fds[0]);
        close(fds[1]);
    }

    #[test]
    fn a_regular_file_is_replaced_by_dev_null() {
        let file = std::fs::File::open(env!("CARGO_MANIFEST_DIR").to_owned() + "/Cargo.toml").unwrap();
        // SAFETY: a new fd onto the same open file, owned by this test.
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert_eq!(detach(fd).unwrap(), Some(ForwardedStdin::RegularFile));
        assert!(is_char_device(fd));
        close(fd);
    }

    /// A character device -- what a terminal is, and what `/dev/null` already is -- is left alone,
    /// and so is an fd that is not open: nothing is opened where there was nothing. (`-1`, not a
    /// number this test closed: the other tests run on other threads and would reuse that one.)
    #[test]
    fn a_character_device_and_a_closed_fd_are_left_alone() {
        let null = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(detach(null.as_raw_fd()).unwrap(), None);
        assert_eq!(detach(-1).unwrap(), None);
    }
}
