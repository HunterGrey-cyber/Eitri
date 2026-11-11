//! What the launcher (`packaging/eitri.launcher.sh`) tells a window start, read first in `main()`.
//!
//! - [`LAUNCHED_FROM_VAR`] is `terminal` or `desktop`, decided by the launcher before anything detached: `desktop`
//!   when none of its standard descriptors was a terminal and it had no controlling terminal. Such a start takes its
//!   `PATH` from the user's login shell (`eitri_core::login_path`), once, before any thread, so the window, nvim, the
//!   bottom terminal, the sidecars and Codex all get the same one.
//! - [`READY_FD_VAR`] names a descriptor the launcher reads when it has put a window started from a terminal in a
//!   session of its own: [`window_up`] writes one line on it once the first window is shown, and from then on the
//!   window's streams no longer reach the launcher, which has gone back to the prompt.
//!
//! - [`NOTE_VAR`] is a line for the window's log the launcher hands on (why a start's scope was given up).
//!
//! All three are taken out of the environment here, so no child sees them. `eitri linked-window` and `eitri
//! run-guard` make inherited descriptors their own before anything else, so for them nothing else is done here.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::Mutex;

/// `terminal` or `desktop`: where the launcher was started from.
pub(crate) const LAUNCHED_FROM_VAR: &str = "EITRI_LAUNCHED_FROM";
/// A line the launcher hands the window for its log, once: why a start's scope was given up.
pub(crate) const NOTE_VAR: &str = "EITRI_LAUNCH_NOTE";
/// The most of a note that is logged.
const NOTE_MAX_CHARS: usize = 400;
/// The number of the descriptor [`window_up`] writes to.
pub(crate) const READY_FD_VAR: &str = "EITRI_LAUNCH_READY_FD";
/// What [`window_up`] writes.
pub(crate) const READY_LINE: &[u8] = b"ready\n";
/// What [`take`] writes as soon as it holds the descriptor: the binary runs, so a start that ends before this line
/// never reached it (a scope the service manager refused), and the launcher may start it once more without one.
pub(crate) const STARTED_LINE: &[u8] = b"started\n";

/// The launcher's descriptor, until [`window_up`] has written to it.
static READY: Mutex<Option<File>> = Mutex::new(None);

/// What a descriptor number in [`READY_FD_VAR`] may be: past the standard three.
pub(crate) fn ready_fd(value: Option<&std::ffi::OsStr>) -> Option<RawFd> {
    let fd: RawFd = value?.to_str()?.parse().ok()?;
    (fd > 2).then_some(fd)
}

/// Whether a start whose first argument is `first` may adopt the ready descriptor and ask the login shell: not the
/// subcommands that take their inherited descriptors as their own first thing.
pub(crate) fn adopts(first: Option<&std::ffi::OsStr>) -> bool {
    !first.is_some_and(|a| a == eitri_core::window_link::start::SUBCOMMAND || a == eitri_core::run_guard::SUBCOMMAND)
}

/// The launcher's note as one log line: control characters become spaces, it is trimmed and cut to
/// [`NOTE_MAX_CHARS`]; `None` when there is nothing to say.
pub(crate) fn note_line(note: Option<&std::ffi::OsStr>) -> Option<String> {
    let text: String = note?
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(NOTE_MAX_CHARS).collect())
}

/// Reads and removes the launcher's variables, asks the login shell when the launcher said `desktop`, and keeps the
/// ready descriptor (closed on exec, so no child holds the launcher's pipe). Returns the lines to log once a print is
/// safe: the launcher's note, then the login shell's. Call it first in `main()`, before any thread: it sets `PATH`.
/// `first` is the command line's first argument.
pub(crate) fn take(first: Option<&std::ffi::OsStr>) -> Vec<String> {
    let launched_from = std::env::var_os(LAUNCHED_FROM_VAR);
    let ready = std::env::var_os(READY_FD_VAR);
    let note = std::env::var_os(NOTE_VAR);
    // Before any other thread exists, so nothing else reads the environment.
    std::env::remove_var(LAUNCHED_FROM_VAR);
    std::env::remove_var(READY_FD_VAR);
    std::env::remove_var(NOTE_VAR);
    if !adopts(first) {
        return Vec::new();
    }
    let mut lines: Vec<String> = note_line(note.as_deref()).into_iter().collect();
    if let Some(fd) = ready_fd(ready.as_deref()) {
        // SAFETY: `fcntl` on a descriptor number; it fails harmlessly when the number names nothing open.
        let open = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0;
        if open {
            // SAFETY: the launcher opened this descriptor for this process alone and nothing else here owns it.
            let mut file = unsafe { File::from_raw_fd(fd) };
            // SAFETY: as above; a write that would block is then dropped rather than holding the GTK thread.
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
            let _ = file.write_all(STARTED_LINE);
            if let Ok(mut ready) = READY.lock() {
                *ready = Some(file);
            }
        }
    }
    let switch = std::env::var_os(eitri_core::login_path::LINUX_SWITCH_VAR);
    if !eitri_core::login_path::wanted(launched_from.as_deref(), switch.as_deref()) {
        return lines;
    }
    let shell = eitri_core::login_path::login_shell(
        std::env::var_os("SHELL").as_deref(),
        &eitri_core::login_path::passwd_shell,
    );
    let found = eitri_core::login_path::ask_login_shell(
        shell,
        eitri_core::login_path::LINUX_BUDGET,
        &eitri_core::login_path::path_from_command,
    );
    if let eitri_core::login_path::LoginPath::FromShell { path, .. } = &found {
        let path: &OsString = path;
        // Still before any other thread exists.
        std::env::set_var("PATH", path);
    }
    lines.push(found.log_line());
    lines
}

/// Whether a launcher reads this window's first output and waits for [`window_up`].
pub(crate) fn launcher_reads() -> bool {
    READY.lock().map(|ready| ready.is_some()).unwrap_or(false)
}

/// Says the window is up to a launcher waiting for it, once, then lets go of what reached the launcher: the
/// descriptor is closed, and the window's standard output and error, which were the launcher's pipe, go on into the
/// window's log alone (`crash_trace::relay::forget_old_destinations`; with no relay, `/dev/null` takes their place).
/// Nothing happens for a start no launcher waits on. A write that fails is dropped: the launcher then says the
/// window did not answer in time, and the window goes on.
pub(crate) fn window_up() {
    let Some(mut ready) = READY.lock().ok().and_then(|mut ready| ready.take()) else {
        return;
    };
    let _ = ready.write_all(READY_LINE);
    drop(ready);
    if !eitri_core::crash_trace::relay::forget_old_destinations() {
        if let Ok(null) = File::options().write(true).open("/dev/null") {
            // SAFETY: `dup2` replaces a standard descriptor atomically, so a print on another thread lands in one
            // place or the other; both descriptors are open.
            unsafe {
                libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
                libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ready_descriptor_is_a_number_past_the_standard_three() {
        use super::*;
        use std::ffi::OsStr;

        assert_eq!(ready_fd(Some(OsStr::new("3"))), Some(3));
        assert_eq!(ready_fd(Some(OsStr::new("17"))), Some(17));
        for bad in ["0", "1", "2", "-4", "x", "", " 3"] {
            assert_eq!(ready_fd(Some(OsStr::new(bad))), None, "{bad:?}");
        }
        assert_eq!(ready_fd(None), None);
    }

    #[test]
    fn the_launchers_note_is_one_bounded_line() {
        use super::*;
        use std::ffi::OsStr;

        assert_eq!(note_line(None), None);
        assert_eq!(note_line(Some(OsStr::new("  \n "))), None);
        assert_eq!(
            note_line(Some(OsStr::new("a scope was refused:\nAccess denied\x1b[0m "))).as_deref(),
            Some("a scope was refused: Access denied [0m")
        );
        let long = "x".repeat(1000);
        assert_eq!(
            note_line(Some(OsStr::new(&long))).map(|l| l.chars().count()),
            Some(NOTE_MAX_CHARS)
        );
    }

    #[test]
    fn the_subcommands_that_take_inherited_descriptors_first_are_left_alone() {
        use super::*;
        use std::ffi::OsStr;

        assert!(!adopts(Some(OsStr::new(eitri_core::window_link::start::SUBCOMMAND))));
        assert!(!adopts(Some(OsStr::new(eitri_core::run_guard::SUBCOMMAND))));
        assert!(!adopts(Some(OsStr::new("linked-window"))));
        assert!(!adopts(Some(OsStr::new("run-guard"))));
        for first in [None, Some(OsStr::new("/a/project")), Some(OsStr::new("choose-project"))] {
            assert!(adopts(first), "{first:?}");
        }
    }
}
