//! A fake `tmux` executable, placed (as a symlink named `tmux`) on the front of the embedded
//! `nvim --embed` child's own `PATH` by `shell/src/pane_switch.rs` -- and *only* that child's
//! `PATH`, never the host process's or any other subprocess's.
//!
//! # Why this exists
//!
//! The project owner already uses `christoomey/vim-tmux-navigator`, which maps `<C-h>`/`<C-l>` to
//! `TmuxNavigateLeft`/`TmuxNavigateRight`. Those commands (see the plugin's own
//! `s:TmuxAwareNavigate`) always try a real `wincmd h`/`wincmd l` first, compare `winnr()` before
//! and after, and only if Neovim had nowhere left to move -- i.e. the cursor is genuinely at a
//! Neovim-internal window boundary -- shell out to
//! `tmux -S <socket> select-pane -t <pane> -<L|R|U|D>`. That "only at the boundary" decision is
//! exactly the behavior `shell` wants for switching between its editor pane and its agent panel,
//! and it is Neovim's to make, not the shell's. So rather than intercepting `Ctrl+h`/`Ctrl+l` at
//! the GTK level (which would break navigation between real `:vsplit` windows), `shell` makes the
//! embedded nvim believe it is running inside tmux, and turns the resulting `select-pane` call
//! into a pane-switch request.
//!
//! The plugin only takes that shell-out path at all when `$TMUX` is non-empty; `shell` sets
//! `TMUX`/`TMUX_PANE` on the nvim child alone, so a real terminal + real tmux + real Neovim
//! session on the same machine is completely unaffected -- those are separate processes with
//! their own real environment.
//!
//! # Contract
//!
//! - Recognizes exactly one argv shape: a `select-pane` invocation carrying one of `-L`/`-R`/`-U`/
//!   `-D`. A trailing `-Z` (`g:tmux_navigator_preserve_zoom`, not set in this config) is
//!   tolerated, as is any other flag ordering, because only the direction token is extracted.
//! - Sends that one direction letter to `shell` over the Unix socket named by
//!   `NEOVIBE_PANE_SWITCH_SOCKET`, then exits.
//! - **Always exits 0**, including when the socket is missing, unreachable, or the argv shape is
//!   unrecognized. This is a UX-enhancement channel, not a security gate like `agent-hook`: the
//!   plugin discards this process's stdout and exit code entirely (`silent call s:TmuxCommand`),
//!   and a failure here must never surface to the user as a broken keystroke. Diagnostics go to
//!   stderr only.
//! - Never reads stdin and never waits for a reply, so it cannot hang the keypress that spawned
//!   it. (`agent-hook` blocks for a real decision because a permission answer is load-bearing;
//!   nothing downstream of a `select-pane` call reads anything back.)
//!
//! Known, accepted limitation: `$TMUX` being set also makes Neovim's *clipboard* provider consider
//! `tmux` as a fallback copy/paste backend (it is last in nvim's own provider list, after
//! `wl-copy`/`xclip`/`xsel`), and this shim answers such a call with silence rather than real
//! clipboard content. On this project's target (a Wayland session with `wl-clipboard` present)
//! `wl-copy` wins long before that fallback is reached. Exiting 0 quietly is still the right
//! failure mode: an unrecognized command must not break the plugin's own control flow.

use std::io::Write;
use std::os::unix::net::UnixStream;

/// Extracts the pane-switch direction from a real `vim-tmux-navigator` argv, or `None` for any
/// other command. Kept as a pure function so it can be unit-tested without a socket or a process.
///
/// `args` is the argument list *without* argv[0]. The real invocation the plugin builds is
/// `-S <socket> select-pane -t <pane> -<L|R|U|D>`; this deliberately does not try to validate the
/// full shape (socket path, pane target), because none of it is information the host needs and
/// being strict would only create new ways to reject a call the plugin considers normal.
fn direction_from_args(args: &[String]) -> Option<char> {
    if !args.iter().any(|arg| arg == "select-pane") {
        return None;
    }
    args.iter().find_map(|arg| match arg.as_str() {
        "-L" => Some('L'),
        "-R" => Some('R'),
        "-U" => Some('U'),
        "-D" => Some('D'),
        _ => None,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Some(direction) = direction_from_args(&args) else {
        // Not a pane-switch call (`display-message`, `run-shell`, a clipboard fallback, ...).
        // Exit 0 silently: the plugin's own control flow must never break on this.
        return;
    };

    let Ok(socket_path) = std::env::var("NEOVIBE_PANE_SWITCH_SOCKET") else {
        eprintln!("neovibe-tmux-shim: NEOVIBE_PANE_SWITCH_SOCKET not set, ignoring {direction}");
        return;
    };

    match UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            if let Err(e) = writeln!(stream, "{direction}") {
                eprintln!("neovibe-tmux-shim: failed to write {direction} to {socket_path}: {e}");
            }
        }
        Err(e) => {
            eprintln!("neovibe-tmux-shim: failed to connect to {socket_path}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extracts_the_direction_from_the_real_plugin_invocation() {
        // Exactly what `s:TmuxCommand` builds for `TmuxNavigateRight` with this project's config
        // (`shellescape($TMUX_PANE)`'s quotes are stripped by the shell before we see argv).
        let a = args(&["-S", "/tmp/neovibe/switch.sock", "select-pane", "-t", "%0", "-R"]);
        assert_eq!(direction_from_args(&a), Some('R'));

        assert_eq!(
            direction_from_args(&args(&["-S", "/s", "select-pane", "-t", "%0", "-L"])),
            Some('L')
        );
        assert_eq!(
            direction_from_args(&args(&["-S", "/s", "select-pane", "-t", "%0", "-U"])),
            Some('U')
        );
        assert_eq!(
            direction_from_args(&args(&["-S", "/s", "select-pane", "-t", "%0", "-D"])),
            Some('D')
        );
    }

    #[test]
    fn tolerates_the_preserve_zoom_flag() {
        // `g:tmux_navigator_preserve_zoom = 1` appends `-Z`. Not set in this project's config, but
        // an extra trailing flag must not make the direction unfindable.
        let a = args(&["-S", "/s", "select-pane", "-t", "%0", "-R", "-Z"]);
        assert_eq!(direction_from_args(&a), Some('R'));
    }

    #[test]
    fn other_tmux_commands_are_not_pane_switches() {
        // The plugin's own zoom check and process-list command, plus a clipboard fallback -- all
        // must fall through to a silent exit 0 rather than being read as a direction.
        assert_eq!(
            direction_from_args(&args(&["-S", "/s", "display-message", "-p", "#{window_zoomed_flag}"])),
            None
        );
        assert_eq!(
            direction_from_args(&args(&["-S", "/s", "run-shell", "ps -o state="])),
            None
        );
        assert_eq!(direction_from_args(&args(&["-S", "/s", "load-buffer", "-"])), None);
        assert_eq!(direction_from_args(&[]), None);
    }

    #[test]
    fn a_direction_flag_without_select_pane_is_ignored() {
        // `-L`/`-R` are not unique to `select-pane`; requiring the subcommand keeps an unrelated
        // tmux call that happens to carry one of those flags from moving the user's focus.
        assert_eq!(direction_from_args(&args(&["-S", "/s", "split-window", "-L"])), None);
    }
}
