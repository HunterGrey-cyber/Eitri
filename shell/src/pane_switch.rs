//! Focus handoff from the editor through `vim-tmux-navigator`.
//! Neovim first runs `wincmd h` or `wincmd l` between its own splits. Only when the
//! window number stays unchanged does the plugin call `tmux select-pane`. This keeps
//! pane focus changes from interrupting navigation within the editor.
//!
//! `TMUX`, `TMUX_PANE` and a shim-prefixed `PATH` are set only on the embedded
//! Neovim child. Process-global environment changes would leak into agent subprocesses
//! and race concurrent environment reads. Other terminal sessions keep their own environment.
//! The shim is exposed as `tmux` in a private directory and sends direction letters
//! through a Unix socket. The core pane-switch module owns the directory, socket,
//! parser and cleanup so each GUI can supply its own event loop.
//!
//! This module locates the shim, polls the socket with a `glib` timer, and supplies
//! the focus grabs wired in `main.rs`.
//!
//! Messages arrive while the editor has focus, after Neovim reaches a split boundary.
//! `R` focuses the agent panel. `L` does nothing at the editor's left edge.
//! `U` focuses the top bar through `main.rs`'s catch-all handler. `D` unzooms and
//! focuses the built-in terminal when shown. If it is hidden, or a Lua panel occupies
//! the bottom slot, `D` reaches the catch-all handler and leaves focus unchanged.
//! The Lua panel in that slot is reachable only by mouse.
//!
//! Navigation from the focused agent panel back to the editor uses a direct GTK
//! capture-phase key handler wired in `main.rs`.
//!
//! If the shim cannot be found or the socket cannot be bound, [`open`] returns `None`.
//! No environment is injected, and the plugin uses plain `wincmd` navigation without
//! calling tmux. The editor continues to work without pane focus handoff.

use std::path::{Path, PathBuf};

use gtk4::glib;

pub(crate) use neovibe_core::pane_switch::PaneSwitchChannel;
use neovibe_core::pane_switch::{accept_pending_directions, sweep_stale_dirs, POLL_INTERVAL};

/// Reclaims stale directories, finds the shim binary and binds the channel, or returns `None`
/// having logged why.
///
/// The sweep is unconditional and first, so it still happens on a machine where the shim binary is
/// missing and this call is about to return `None`.
pub(crate) fn open() -> Option<PaneSwitchChannel> {
    sweep_stale_dirs();
    let shim = locate_shim_binary()?;
    PaneSwitchChannel::bind(&shim)
}

/// Starts polling the socket from the GTK main loop, invoking `on_direction` with one of
/// `'L'`/`'R'`/`'U'`/`'D'` for each message the shim sends.
///
/// A poll loop on the main thread rather than a background thread with a blocking `accept()`:
/// the callback has to touch GTK widgets (grabbing focus), which is main-thread-only anyway,
/// and `agent`'s own hook listener already paid for the lesson that a blocking `accept()` with
/// no stop signal is a real hang waiting to happen.
///
/// Takes the channel's listener, so a second call on the same channel logs and does nothing rather
/// than installing a second timer that would race the first for every connection.
pub(crate) fn listen(channel: &mut PaneSwitchChannel, on_direction: impl Fn(char) + 'static) {
    let Some(listener) = channel.take_listener() else {
        eprintln!("[pane_switch] listen() called twice -- ignoring");
        return;
    };
    glib::timeout_add_local(POLL_INTERVAL, move || {
        for direction in accept_pending_directions(&listener) {
            on_direction(direction);
        }
        glib::ControlFlow::Continue
    });
}

/// Finds the `neovibe-tmux-shim` binary next to the currently-running executable. Both are
/// binaries of the same Cargo package, so they always land in the same directory -- `target/debug`
/// during development, a single `bin/` directory for any real install.
///
/// Stays in `shell` rather than moving into the core protocol with everything else: which binary
/// plays `tmux`, and where a host keeps it, is that host's packaging, not the wire.
fn locate_shim_binary() -> Option<PathBuf> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[pane_switch] current_exe() failed: {e} -- Ctrl+h/Ctrl+l pane switching disabled");
            return None;
        }
    };
    let candidate = exe.parent()?.join("neovibe-tmux-shim");
    if !Path::new(&candidate).is_file() {
        eprintln!("[pane_switch] {} not found -- Ctrl+h/Ctrl+l pane switching disabled (build it with `cargo build -p shell`)", candidate.display());
        return None;
    }
    Some(candidate)
}
