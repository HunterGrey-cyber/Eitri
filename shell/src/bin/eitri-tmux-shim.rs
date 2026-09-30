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
//!   `EITRI_PANE_SWITCH_SOCKET`, then exits.
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

use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

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

/// Spec §6.3 (P9): whether a typed `tmux` inside nvim's own `:terminal` deserves a message on
/// stderr, and what it says. `None` for a real pane-switch call (`select-pane -<dir>`), because
/// that is `vim-tmux-navigator` doing its job, and `None` for any call whose stderr is not a
/// terminal -- the plugin's own calls run through `system()` with piped fds, never a terminal, and
/// must keep exiting 0 silently (the shim's contract: a failure here must never surface as a broken
/// keystroke). Only a human typing `tmux` at a real prompt inside `:terminal` sees this. Kept pure
/// (no process exit, no env lookups of its own) so it is unit-testable without a process.
fn notice_for(
    args: &[String],
    stderr_is_tty: bool,
    path_var: Option<&str>,
    own_dir: &Path,
    shim: Option<&Path>,
) -> Option<String> {
    if !stderr_is_tty || direction_from_args(args).is_some() {
        return None;
    }
    Some(format!(
        "eitri: this `tmux` is Eitri's pane-switch shim (TMUX is set for nvim inside Eitri).\n\
         Run the real tmux with:  env -u TMUX {}",
        real_tmux_after(path_var, own_dir, shim)
    ))
}

/// The shim's own directory, from something that does not move: `EITRI_PANE_SWITCH_SOCKET`'s
/// parent joined with `bin`, the channel's own layout (`eitri_core::pane_switch`: the socket and
/// the `bin/tmux` symlink sit in one private directory). `PATH`'s leading entry is only the
/// fallback when that variable is gone: it is the shim's directory for the nvim child itself
/// (`PaneSwitchChannel::child_env` prepends it), but not inside nvim's `:terminal` once an
/// interactive shell's rc file prepends a directory of its own (`export PATH="$HOME/.local/bin:$PATH"`),
/// which is exactly where a human types `tmux` (whole-branch review of v1-ui).
fn own_dir_from(socket_var: Option<&OsStr>, path_var: Option<&str>) -> PathBuf {
    if let Some(parent) = socket_var.map(Path::new).and_then(Path::parent) {
        if !parent.as_os_str().is_empty() {
            return parent.join("bin");
        }
    }
    path_var
        .and_then(|p| std::env::split_paths(p).next())
        .unwrap_or_default()
}

/// The first real `tmux` on `path_var` after the shim's own place on it, or `"tmux is not
/// installed"` if none (spec §6.3). The shim's place is `own_dir` ([`own_dir_from`]), or any entry
/// whose `tmux` is `shim` itself; and a candidate that resolves to `shim` is never offered, however
/// it got onto `PATH` -- `is_file()` follows the `bin/tmux` symlink, so without this a directory
/// the shell prepended would make the walk start early and name the shim as the real tmux. `shim`
/// is this binary, canonicalized (`None` if that cannot be read: then only `own_dir` places it).
/// Only checks the entry is a file, the same bar `pane_switch::locate_shim_binary` holds this
/// shim's own binary to.
fn real_tmux_after(path_var: Option<&str>, own_dir: &Path, shim: Option<&Path>) -> String {
    let Some(path_var) = path_var else {
        return "tmux is not installed".to_string();
    };
    let is_shim = |candidate: &Path| shim.is_some_and(|shim| candidate.canonicalize().is_ok_and(|c| c == shim));
    let mut past_own_dir = false;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join("tmux");
        if !past_own_dir {
            past_own_dir = dir == own_dir || is_shim(&candidate);
            continue;
        }
        if candidate.is_file() && !is_shim(&candidate) {
            return candidate.display().to_string();
        }
    }
    "tmux is not installed".to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let path_var = std::env::var("PATH").ok();
    let own_dir = own_dir_from(
        std::env::var_os("EITRI_PANE_SWITCH_SOCKET").as_deref(),
        path_var.as_deref(),
    );
    let shim = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
    if let Some(notice) = notice_for(
        &args,
        std::io::stderr().is_terminal(),
        path_var.as_deref(),
        &own_dir,
        shim.as_deref(),
    ) {
        eprintln!("{notice}");
        std::process::exit(1);
    }

    let Some(direction) = direction_from_args(&args) else {
        // Not a pane-switch call (`display-message`, `run-shell`, a clipboard fallback, ...).
        // Exit 0 silently: the plugin's own control flow must never break on this.
        return;
    };

    let Ok(socket_path) = std::env::var("EITRI_PANE_SWITCH_SOCKET") else {
        eprintln!("eitri-tmux-shim: EITRI_PANE_SWITCH_SOCKET not set, ignoring {direction}");
        return;
    };

    match UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            if let Err(e) = writeln!(stream, "{direction}") {
                eprintln!("eitri-tmux-shim: failed to write {direction} to {socket_path}: {e}");
            }
        }
        Err(e) => {
            eprintln!("eitri-tmux-shim: failed to connect to {socket_path}: {e}");
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
        let a = args(&["-S", "/tmp/eitri/switch.sock", "select-pane", "-t", "%0", "-R"]);
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

    /// A scratch directory under `TMPDIR`, unique to this test's name and this process, so
    /// concurrent test runs (and a leftover from a killed one) can never collide. Removed first,
    /// in case a previous run of this same binary left it behind.
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nv-tmux-shim-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a writable scratch dir under TMPDIR");
        dir
    }

    /// Spec §6.3: a real `select-pane` call never gets the notice, tty or not -- it is
    /// `vim-tmux-navigator` doing exactly its job.
    #[test]
    fn a_pane_switch_call_never_gets_a_notice() {
        let a = args(&["-S", "/s", "select-pane", "-t", "%0", "-R"]);
        assert_eq!(
            notice_for(&a, true, Some("/usr/bin"), Path::new("/own/bin"), None),
            None
        );
        assert_eq!(
            notice_for(&a, false, Some("/usr/bin"), Path::new("/own/bin"), None),
            None
        );
    }

    /// Spec §6.3: the plugin's other calls (a zoom check, a clipboard fallback, ...) run through
    /// `system()` with piped fds -- never a terminal -- so they keep exiting silently too.
    #[test]
    fn a_non_pane_switch_call_off_a_terminal_gets_no_notice() {
        let a = args(&["-S", "/s", "display-message", "-p", "#{window_zoomed_flag}"]);
        assert_eq!(
            notice_for(&a, false, Some("/usr/bin"), Path::new("/own/bin"), None),
            None
        );
    }

    /// The one case that does get a notice: a human types `tmux` at a real prompt inside `:terminal`.
    /// It names the first real `tmux` strictly after the shim's own directory on `PATH`.
    #[test]
    fn a_typed_tmux_on_a_real_terminal_names_the_real_tmux_after_own_dir() {
        let root = scratch_dir("found");
        let own_dir = root.join("bin");
        let real_dir = root.join("usr-bin");
        std::fs::create_dir_all(&own_dir).expect("fixture");
        std::fs::create_dir_all(&real_dir).expect("fixture");
        std::fs::write(real_dir.join("tmux"), b"").expect("fixture");
        let path_var = format!("{}:{}", own_dir.display(), real_dir.display());

        let a = args(&["-S", "/s", "display-message", "-p", "x"]);
        let notice = notice_for(&a, true, Some(&path_var), &own_dir, None).expect("a notice on a real tty call");
        assert!(
            notice.starts_with("eitri: this `tmux` is Eitri's pane-switch shim (TMUX is set for nvim inside Eitri)."),
            "got {notice:?}"
        );
        assert!(
            notice.ends_with(&format!(
                "Run the real tmux with:  env -u TMUX {}",
                real_dir.join("tmux").display()
            )),
            "got {notice:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// No `tmux` anywhere after the shim's own directory: the notice says so rather than naming a
    /// path that does not exist, and a missing `PATH` altogether is the same case.
    #[test]
    fn no_real_tmux_after_own_dir_says_none_is_installed() {
        let root = scratch_dir("missing");
        let own_dir = root.join("bin");
        let other_dir = root.join("nothing-here");
        std::fs::create_dir_all(&own_dir).expect("fixture");
        std::fs::create_dir_all(&other_dir).expect("fixture");
        let path_var = format!("{}:{}", own_dir.display(), other_dir.display());

        assert_eq!(
            real_tmux_after(Some(&path_var), &own_dir, None),
            "tmux is not installed"
        );
        assert_eq!(real_tmux_after(None, &own_dir, None), "tmux is not installed");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real `tmux` *before* the shim's own directory on `PATH` is never offered: only entries
    /// strictly after `own_dir` count, since anything earlier is not what a human's `$PATH` search
    /// would have reached the shim through in the first place.
    #[test]
    fn a_tmux_entry_before_own_dir_is_never_offered() {
        let root = scratch_dir("before");
        let earlier = root.join("earlier");
        let own_dir = root.join("bin");
        std::fs::create_dir_all(&earlier).expect("fixture");
        std::fs::create_dir_all(&own_dir).expect("fixture");
        std::fs::write(earlier.join("tmux"), b"").expect("fixture");
        let path_var = format!("{}:{}", earlier.display(), own_dir.display());

        assert_eq!(
            real_tmux_after(Some(&path_var), &own_dir, None),
            "tmux is not installed"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Whole-branch review of v1-ui: inside nvim's `:terminal`, an interactive shell's rc file
    /// prepends its own directory (`export PATH="$HOME/.local/bin:$PATH"`), so `PATH`'s leading entry
    /// is no longer the shim's. Walking from there reached the shim's own `bin/tmux` symlink first
    /// and named it as "the real tmux". The shim's directory comes from the socket now, and a
    /// candidate that is the shim itself is never offered even when the walk starts early.
    #[test]
    fn a_directory_the_shell_prepended_never_makes_the_shim_the_real_tmux() {
        let root = scratch_dir("prepended");
        let channel = root.join("nv-ps-1");
        let own_dir = channel.join("bin");
        let local = root.join("local-bin");
        let real_dir = root.join("usr-bin");
        for dir in [&own_dir, &local, &real_dir] {
            std::fs::create_dir_all(dir).expect("fixture");
        }
        let shim = root.join("eitri-tmux-shim");
        std::fs::write(&shim, b"").expect("fixture");
        std::os::unix::fs::symlink(&shim, own_dir.join("tmux")).expect("fixture");
        std::fs::write(real_dir.join("tmux"), b"").expect("fixture");
        let shim = shim.canonicalize().expect("fixture");
        let real = real_dir.join("tmux").display().to_string();
        let path_var = format!("{}:{}:{}", local.display(), own_dir.display(), real_dir.display());

        let socket = channel.join("s.sock");
        let derived = own_dir_from(Some(socket.as_os_str()), Some(&path_var));
        assert_eq!(derived, own_dir, "the socket's directory, not PATH's first entry");
        assert_eq!(real_tmux_after(Some(&path_var), &derived, Some(&shim)), real);

        // The old derivation (PATH's first entry) with the shim known: its symlink is skipped.
        assert_eq!(real_tmux_after(Some(&path_var), &local, Some(&shim)), real);
        // The shim's directory placed by its symlink alone, with no own_dir on PATH at all.
        assert_eq!(
            real_tmux_after(Some(&path_var), Path::new("/nowhere"), Some(&shim)),
            real
        );
        // Nothing past the shim but the shim again: none is installed.
        let only_shim = format!("{}:{}", local.display(), own_dir.display());
        assert_eq!(
            real_tmux_after(Some(&only_shim), &local, Some(&shim)),
            "tmux is not installed"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no socket variable (or one with no parent), the shim's directory falls back to
    /// `PATH`'s leading entry -- what `PaneSwitchChannel::child_env` gives the nvim child itself.
    #[test]
    fn without_the_socket_variable_the_shim_dir_is_paths_first_entry() {
        assert_eq!(own_dir_from(None, Some("/a/bin:/usr/bin")), PathBuf::from("/a/bin"));
        assert_eq!(
            own_dir_from(Some(OsStr::new("s.sock")), Some("/a/bin:/usr/bin")),
            PathBuf::from("/a/bin")
        );
        assert_eq!(own_dir_from(None, None), PathBuf::new());
        assert_eq!(
            own_dir_from(Some(OsStr::new("/t/nv-ps-9/s.sock")), Some("/x:/y")),
            PathBuf::from("/t/nv-ps-9/bin")
        );
    }
}
