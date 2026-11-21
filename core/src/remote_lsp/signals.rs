//! Signals as bytes on a pipe: a handler that only writes the signal's number to a pipe, whose read end a loop
//! polls or a thread reads. Shared by the language-server bridge (`eitri host lsp`) and the server shell
//! (`eitri host term`); one verb runs per process, and the handlers' pipe is the process's, so [`watch`] takes it once
//! and refuses a second call.

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The write end of the pipe the handler writes to (-1 before it is made).
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);
/// Whether [`watch`] has been called in this process.
static WATCHED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = signal as u8;
    // SAFETY: only async-signal-safe calls: errno is saved and put back around a write(2) of one byte to a
    // nonblocking pipe this process made (a full pipe drops the byte; one is already waiting).
    unsafe {
        let saved = *errno();
        libc::write(fd, (&byte as *const u8).cast(), 1);
        *errno() = saved;
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno storage.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno storage.
    unsafe { libc::__error() }
}

/// Makes `signals` arrive as one byte each (the signal's number) on the returned pipe's read end, which is blocking and
/// close-on-exec: read it after a poll says it is readable, or on a thread of its own. A handler (not a blocked mask)
/// so that the processes this one starts (ssh, a locally run server) get the signals' usual dispositions back at their
/// `exec`; `SA_RESTART`, so that the blocking reads of other threads go on.
///
/// Once per process: a second call fails with `AlreadyExists` and changes nothing. The caller makes this call before
/// it starts any process (pipe(2) and fcntl(2) rather than pipe2, which macOS lacks, leave a moment in which an exec
/// on another thread would inherit the descriptors).
pub fn watch(signals: &[libc::c_int]) -> std::io::Result<OwnedFd> {
    if WATCHED.swap(true, Ordering::SeqCst) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "signals are already watched in this process",
        ));
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe writes two new descriptors into `fds`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just made and nothing else owns them.
    let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read_end, &write_end] {
        // SAFETY: fcntl only changes the flags of a descriptor this function owns.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // SAFETY: as above, on the write end, so that a full pipe never makes the handler wait.
    if unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The write end stays open for the life of the process: the handler may run late.
    SIGNAL_PIPE.store(write_end.into_raw_fd(), Ordering::Relaxed);
    for &signal in signals {
        // SAFETY: a zeroed sigaction with a handler that only makes async-signal-safe calls.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(read_end)
}
