//! Focus handoff between the editor and neighbouring modules uses Neovim's navigation
//! plugin so movement between Neovim splits happens before crossing a module boundary.
//!
//! This module finds the shim binary and wires focus grabs; `editor_feeds::FeedPump` drains
//! the socket.
//! The shared channel handles the socket directory, `tmux` symlink, parser and cleanup,
//! so another host can use the same protocol with its own event loop.
//!
//! The navigation plugin first runs `wincmd`, then compares `winnr()` before and after.
//! Only at a Neovim window boundary, and with `$TMUX` set, does it invoke
//! `tmux -S <socket> select-pane -t <pane> -<L|R|U|D>`.
//!
//! `TMUX`, `TMUX_PANE` and the prefixed `PATH` are set only on the embedded nvim child through
//! `child_env`. Process-global environment changes would affect agent subprocesses and race
//! concurrent environment reads. The child's `PATH` finds a `tmux` symlink to the shim binary.
//! The shim sends a direction letter to the Unix socket, which the GTK main loop drains into
//! the host's callback. Other process trees keep their own environments.
//!
//! Direction letters are mapped by [`letter_direction`] and routed using layout geometry.
//! In the default `[editor | agent]` layout, `R` reaches the agent, `U` the top bar, and `L` or
//! `D` does nothing at the outer edge. With a visible module below the editor, `D` unzooms and
//! focuses it. A hidden bottom terminal is absent from the geometry, so `D` is an edge no-op.
//!
//! When a web module has focus, its host's module key stage (`gtk_host::keys::module_key`)
//! asks the window controller to navigate using the same geometry.
//!
//! If the shim cannot be found or the socket cannot be bound, [`open`] returns `None` and no
//! environment is injected. The editor still supports normal navigation between Neovim splits.

use std::path::{Path, PathBuf};

use eitri_core::pane_switch::sweep_stale_dirs;
pub(crate) use eitri_core::pane_switch::{letter_direction, PaneMessage, PaneSwitchChannel};

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

/// Finds the `eitri-tmux-shim` binary next to the currently-running executable. Both are
/// binaries of the same Cargo package, so they always land in the same directory -- `target/debug`
/// during development, a single `bin/` directory for any real install.
///
/// Stays in `shell` rather than moving into the core protocol with everything else: which binary
/// plays `tmux`, and where a host keeps it, is that host's packaging, not the wire.
fn locate_shim_binary() -> Option<PathBuf> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[pane_switch] current_exe() failed: {e} -- Ctrl+h/j/k/l out of nvim disabled");
            return None;
        }
    };
    let candidate = exe.parent()?.join("eitri-tmux-shim");
    if !Path::new(&candidate).is_file() {
        eprintln!(
            "[pane_switch] {} not found -- Ctrl+h/j/k/l out of nvim disabled (build it with `cargo build -p shell`)",
            candidate.display()
        );
        return None;
    }
    Some(candidate)
}
