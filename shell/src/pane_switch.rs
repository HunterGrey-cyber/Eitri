//! `Ctrl+h`/`Ctrl+l` focus handoff between the editor pane and the agent panel, implemented by
//! *leveraging* the user's real `christoomey/vim-tmux-navigator` rather than intercepting the
//! keys at the GTK level.
//!
//! This module is the GTK half: locating the shim binary, the `glib` timer that drains the socket,
//! and the focus grabs `main.rs` wires to each direction. The protocol under it -- the private
//! directory, the fake-`tmux` symlink, the socket, the parser, the cleanup -- is
//! `neovibe_core::pane_switch`, moved there in L2 T5 (2026-09-17) so a second host can reuse it
//! with its own run-loop timer.
//!
//! # The mechanism, and why it is shaped this way
//!
//! `vim-tmux-navigator`'s `s:TmuxAwareNavigate` (read in full at
//! `~/.local/share/nvim/lazy/vim-tmux-navigator/plugin/tmux_navigator.vim`, lines 121-158) does
//! three things in order:
//!
//! 1. runs a real `wincmd h`/`wincmd l` -- so navigation between real Neovim `:vsplit` windows
//!    keeps working exactly as it always has;
//! 2. compares `winnr()` before and after, to detect that Neovim had nowhere left to go;
//! 3. **only then**, and only if `$TMUX` is non-empty, shells out to
//!    `tmux -S <socket> select-pane -t <pane> -<L|R|U|D>`.
//!
//! Step 2 is the whole point. A hardcoded GTK-level `Ctrl+l` intercept in `shell` would jump to
//! the agent panel even when the user only meant to move to the split on their right; the
//! boundary decision belongs to Neovim, which is the only party that knows its own window layout.
//! So instead of taking that decision away, `shell` makes the embedded nvim believe it is running
//! inside tmux and turns its resulting `select-pane` call into a focus switch:
//!
//! - `TMUX` and `TMUX_PANE` are set on the `nvim --embed` child process **only** (via
//!   `NeovideEditorPaneOptions::child_env` -> `LiveHarnessOptions::child_env` ->
//!   `CmdLineSettings::child_env` -> `Command::env`), never on this process. `std::env::set_var`
//!   is deliberately not used: it mutates process-global state, would leak into the `claude`
//!   subprocesses `agent` spawns, and races concurrent `getenv` in a multi-threaded GTK app.
//! - `PATH` for that same child is prefixed with a private directory whose only entry is a
//!   symlink named `tmux` pointing at this workspace's own `neovibe-tmux-shim` binary
//!   (`shell/src/bin/neovibe-tmux-shim.rs`).
//! - The shim writes one direction letter to the Unix socket named by
//!   `NEOVIBE_PANE_SWITCH_SOCKET`; [`listen`] polls that socket from the GTK main loop and hands
//!   the letter to the host's callback.
//!
//! Because all three are per-child environment variables, a real terminal + real tmux + real
//! Neovim session on the same machine is untouched -- it is a different process tree with its own
//! real environment.
//!
//! # Directions
//!
//! A message only ever arrives while the *editor* has focus (that is the only context in which
//! the Neovim plugin's mappings are live) and Neovim has already hit its own boundary:
//!
//! - `R` -- the load-bearing case: give focus to the agent panel.
//! - `L` -- harmless no-op; the editor is already the leftmost pane, which is exactly what real
//!   tmux does when you press `Ctrl+h` in the leftmost tmux pane.
//! - `U`/`D` -- both no-ops. `D` used to reach the native terminal pane in the bottom slot; that
//!   pane was frozen out on 2026-09-19 (`freeze/terminal-stack`) and its `'D'` match arm went with
//!   it. A Lua-registered panel can still occupy the bottom slot, but nothing routes focus into it
//!   -- the arm was never wired for plugin panels -- so `'D'` now falls into `main.rs`'s catch-all
//!   and prints "no pane in that direction, ignoring". The slot is mouse-reachable only.
//!
//! The opposite direction (agent panel focused, `Ctrl+h` back to the editor) does **not** go
//! through this mechanism -- the Neovim plugin has no relevance while a `WebView` has focus. That
//! is a direct GTK capture-phase key handler, wired in `main.rs`.
//!
//! # Failure mode
//!
//! Every step degrades to "the feature is simply absent": if the shim binary can't be located, or
//! the socket can't be bound, [`open`] returns `None`, no environment is injected, the
//! embedded nvim never sees `$TMUX`, and `vim-tmux-navigator` behaves exactly as it does outside
//! tmux (plain `wincmd` wrappers, no shell-out). Nothing about the editor breaks.

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
