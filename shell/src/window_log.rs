//! Starts what a window leaves behind: `eitri_core::crash_trace`'s panic records and its log.

use std::os::fd::AsRawFd;
use std::path::Path;

use eitri_core::crash_trace::{self, Route};
use eitri_core::window_link::names::WindowName;

use crate::version;

/// Installs the panic hook unless the switch turns it off, and says whether it did.
fn install() -> bool {
    if crash_trace::is_off(std::env::var_os(crash_trace::OFF_ENV).as_deref()) {
        return false;
    }
    let (state, home) = (std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"));
    let dir = crash_trace::log_dir(state.as_deref(), home.as_deref());
    crash_trace::install(&version::version_line(), dir.as_deref(), home.as_deref());
    true
}

/// A main window, before its project is known: every print from here on is safe, and a panic leaves a record. The
/// threads this starts never read the environment, which `main()` goes on setting.
pub(crate) fn start_early() {
    if install() {
        crash_trace::keep_output(None, crash_trace::route_for(1), crash_trace::route_for(2));
    }
}

/// The main window's log, now that its project is known, and the line a record from an earlier run is owed. A window on
/// a terminal has no log: the relay carries nothing and the terminal has it all.
pub(crate) fn start_main(project_root: &Path) {
    if crash_trace::is_off(std::env::var_os(crash_trace::OFF_ENV).as_deref()) {
        return;
    }
    let (state, home) = (std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"));
    let Some(dir) = crash_trace::log_dir(state.as_deref(), home.as_deref()) else {
        return;
    };
    if crash_trace::relay::running() {
        if let Some((file, path)) = crash_trace::open_main_log(&dir, project_root) {
            crash_trace::attach_log(file, path);
        }
    }
    crash_trace::set_pending_notice(crash_trace::new_crash_notice(&dir, home.as_deref()));
}

/// Window 2 or a module window: its streams are `/dev/null` from the spawn, so the log is all they have. Writes to a
/// log can fail, and these are made from GTK callbacks, so the log gets the relay; should the relay not start, a
/// plain `dup2` is still a log.
pub(crate) fn start_linked(state_home: Option<&Path>, root: &Path, window: &WindowName) {
    if !install() {
        return;
    }
    let log = state_home.and_then(|home| eitri_core::window_link::start::open_log(home, root, window).ok());
    let Some((file, path)) = log else {
        crash_trace::keep_output(None, Route::Relay { tee: false }, Route::Relay { tee: false });
        return;
    };
    let copy = file.try_clone().ok();
    let quiet = Route::Relay { tee: false };
    if !crash_trace::keep_output(Some((file, path)), quiet, quiet) {
        if let Some(log) = copy {
            // SAFETY: `dup2` onto the standard descriptors, from a descriptor open for the call. Nothing else in the
            // process runs yet, so nothing is writing to either.
            unsafe {
                libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
                libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
            }
        }
    }
}

/// A companion panel. Started from an nvim it leaves that editor's pipes; otherwise its output is kept as a main
/// window's is.
pub(crate) fn start_companion(project_root: &Path, from_editor: bool) {
    let on = install();
    if from_editor {
        crate::companion::leave_editor_pipes(project_root);
    } else if on {
        let (state, home) = (std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"));
        let dir = eitri_core::companion::stdio::log_dir(state.as_deref(), home.as_deref());
        let log = dir
            .as_deref()
            .and_then(|dir| eitri_core::companion::stdio::open_log(dir, project_root));
        crash_trace::keep_output(log, crash_trace::route_for(1), crash_trace::route_for(2));
    }
}

#[cfg(feature = "probe")]
pub(crate) mod probe {
    //! `EITRI_CRASH_PROBE`, in a `probe` build only: a panic on demand in a real window, for the sandbox pass.

    /// Text a record must never show; the words are the probe's own.
    const SECRET: &str = "crash-probe-secret-words";

    pub(crate) fn arm() {
        let Some(kind) = std::env::var_os("EITRI_CRASH_PROBE") else {
            return;
        };
        let kind = kind.to_string_lossy().into_owned();
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || match kind.as_str() {
            "window" => panic!("probe: {SECRET}"),
            "thread" => {
                let _ = std::thread::Builder::new()
                    .name("crash-probe".to_string())
                    .spawn(|| panic!("probe: {SECRET}"));
            }
            "slice" => {
                let text = format!("é{SECRET}");
                println!("[probe] about to cut {}", text.len());
                let _ = &text[1..];
            }
            "print" => println!("[probe] a line after the disk filled"),
            other => eprintln!("[probe] unknown crash probe {other:?}"),
        });
    }
}
