//! `Ctrl+h`/`Ctrl+l` focus handoff between the editor pane and the agent panel, implemented by
//! *leveraging* the user's real `christoomey/vim-tmux-navigator` rather than intercepting the
//! keys at the GTK level.
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
//!   `NEOVIBE_PANE_SWITCH_SOCKET`; [`PaneSwitch::listen`] polls that socket from the GTK main
//!   loop and hands the letter to the host's callback.
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
//! - `U`/`D` -- always no-ops; `shell` has no vertical pane layout at all.
//!
//! The opposite direction (agent panel focused, `Ctrl+h` back to the editor) does **not** go
//! through this mechanism -- the Neovim plugin has no relevance while a `WebView` has focus. That
//! is a direct GTK capture-phase key handler, wired in `main.rs`.
//!
//! # Failure mode
//!
//! Every step degrades to "the feature is simply absent": if the shim binary can't be located, or
//! the socket can't be bound, [`PaneSwitch::new`] returns `None`, no environment is injected, the
//! embedded nvim never sees `$TMUX`, and `vim-tmux-navigator` behaves exactly as it does outside
//! tmux (plain `wincmd` wrappers, no shell-out). Nothing about the editor breaks.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gtk4::glib;

/// How often the GTK main loop checks the socket for a pending shim connection. A keypress-driven
/// focus switch at this cadence is imperceptible, and an idle poll of a non-blocking `accept()`
/// costs one failing syscall.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bound on how long a single accepted connection may block the GTK main loop while its one short
/// line arrives. The shim writes immediately after `connect()` returns, so in practice this is
/// never reached; it exists because an *accepted* `UnixStream` does not inherit its listener's
/// non-blocking flag on Linux -- the exact bug that cost `agent`'s hook listener a real debugging
/// pass (see `agent/src/process.rs`), reproduced here as a deliberate precaution rather than
/// rediscovered.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// The directory-name prefix `instance_dir` keys this module's directories on.
const SHIM_DIR_PREFIX: &str = "neovibe-pane-switch-";

/// A live pane-switch channel: a private directory holding the fake-`tmux` symlink and the Unix
/// socket, plus the bound listener.
///
/// Both `Drop` and the explicit [`PaneSwitch::cleanup`] remove that whole directory, but **neither
/// is guaranteed to run**, so this type does not promise that nothing is left behind. In `shell`
/// the value lives inside the window's `close-request` closure, which GTK was observed not to free
/// before the process exits (see [`PaneSwitch::cleanup`]'s own doc for that finding, and why the
/// close handler calls it explicitly), and no destructor runs at all on SIGKILL or a hard crash.
/// What survives such an exit is a dead symlink and a dead socket under `TMPDIR`;
/// `crate::instance_dir::sweep_stale_instance_dirs`, which runs at the top of [`PaneSwitch::new`],
/// is what reclaims them on a later launch.
pub(crate) struct PaneSwitch {
    dir: PathBuf,
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl PaneSwitch {
    /// Builds the shim directory and binds the socket, or returns `None` (having logged why) if
    /// anything needed is missing. `None` is a supported outcome, not an error path: the caller
    /// simply injects no environment and the embedded nvim behaves as it does outside tmux.
    pub(crate) fn new() -> Option<Self> {
        // Reclaim whatever earlier, now-dead `shell` processes left behind before adding one more
        // directory of our own. Unconditional and first, so it still happens on a machine where
        // the shim binary is missing and this call is about to return `None`.
        crate::instance_dir::sweep_stale_instance_dirs(&std::env::temp_dir(), SHIM_DIR_PREFIX, "switch.sock", "pane_switch");

        let shim = locate_shim_binary()?;

        let dir = shim_dir_path();
        let bin_dir = dir.join("bin");
        // No `remove_dir_all` first: `shim_dir_path` returns a path that has never existed (see
        // its own doc), so there is nothing to clear -- and an unconditional recursive delete of a
        // `TMPDIR`-derived path this process has not yet created is worth not having at all.
        if let Err(e) = std::fs::create_dir_all(&bin_dir) {
            eprintln!("[pane_switch] could not create {}: {e} -- Ctrl+h/Ctrl+l pane switching disabled", bin_dir.display());
            return None;
        }

        let fake_tmux = bin_dir.join("tmux");
        if let Err(e) = std::os::unix::fs::symlink(&shim, &fake_tmux) {
            eprintln!("[pane_switch] could not link {} -> {}: {e} -- Ctrl+h/Ctrl+l pane switching disabled", fake_tmux.display(), shim.display());
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }

        let socket_path = dir.join("switch.sock");
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("[pane_switch] could not bind {}: {e} -- Ctrl+h/Ctrl+l pane switching disabled", socket_path.display());
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
        };
        if let Err(e) = listener.set_nonblocking(true) {
            eprintln!("[pane_switch] could not set the switch socket non-blocking: {e} -- Ctrl+h/Ctrl+l pane switching disabled");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }

        println!("[pane_switch] fake tmux at {}, socket at {}", fake_tmux.display(), socket_path.display());
        Some(Self { dir, socket_path, listener: Some(listener) })
    }

    /// The `(name, value)` pairs to hand to `NeovideEditorPaneOptions::child_env`. **These must
    /// never be applied to this process** -- see this module's own doc.
    ///
    /// `TMUX`'s value follows real tmux's own `<socket>,<pid>,<session>` shape, and is what
    /// `vim-tmux-navigator`'s `s:TmuxSocket()` splits on `,` to build its `-S` argument; the shim
    /// ignores `-S` entirely, so only the "non-empty, and does not contain the substring `tmate`"
    /// part is load-bearing (that substring is what the plugin's `s:TmuxOrTmateExecutable` tests
    /// to decide whether to invoke `tmate` instead of `tmux`).
    pub(crate) fn child_env(&self) -> Vec<(String, String)> {
        let bin_dir = self.dir.join("bin");
        let path = match std::env::var("PATH") {
            Ok(existing) => format!("{}:{}", bin_dir.display(), existing),
            Err(_) => bin_dir.display().to_string(),
        };
        vec![
            ("TMUX".to_string(), format!("{},{},0", self.socket_path.display(), std::process::id())),
            ("TMUX_PANE".to_string(), "%0".to_string()),
            ("PATH".to_string(), path),
            ("NEOVIBE_PANE_SWITCH_SOCKET".to_string(), self.socket_path.display().to_string()),
        ]
    }

    /// Starts polling the socket from the GTK main loop, invoking `on_direction` with one of
    /// `'L'`/`'R'`/`'U'`/`'D'` for each message the shim sends. Consumes the listener, so this can
    /// only be called once.
    ///
    /// A poll loop on the main thread rather than a background thread with a blocking `accept()`:
    /// the callback has to touch GTK widgets (grabbing focus), which is main-thread-only anyway,
    /// and `agent`'s own hook listener already paid for the lesson that a blocking `accept()` with
    /// no stop signal is a real hang waiting to happen.
    pub(crate) fn listen(&mut self, on_direction: impl Fn(char) + 'static) {
        let Some(listener) = self.listener.take() else {
            eprintln!("[pane_switch] listen() called twice -- ignoring");
            return;
        };
        glib::timeout_add_local(POLL_INTERVAL, move || {
            // Drain everything already queued rather than one per tick: a fast repeated keypress
            // can leave more than one connection pending between polls.
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                        let mut line = String::new();
                        if BufReader::new(stream).read_line(&mut line).is_err() {
                            eprintln!("[pane_switch] failed to read a direction from an accepted connection");
                            continue;
                        }
                        match parse_direction(&line) {
                            Some(direction) => on_direction(direction),
                            None => eprintln!("[pane_switch] ignoring unrecognized message {line:?}"),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        eprintln!("[pane_switch] accept failed: {e}");
                        break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
}

impl PaneSwitch {
    /// Removes the shim directory (fake `tmux` symlink and socket alike). Idempotent, and safe to
    /// call while the nvim child is still alive -- by the time a host calls this it is shutting
    /// that child down anyway, and a missing `tmux` only means the plugin's `system()` call finds
    /// nothing to run.
    ///
    /// **This exists because `Drop` alone was observed not to be enough.** In the sandbox, closing
    /// the real `shell` window left the shim directory behind every time: the `PaneSwitch` is
    /// owned by the window's `close-request` closure, and GTK does not deterministically free
    /// signal-handler closures before the process exits, so its `Drop` never ran. That matters
    /// more now than it did: a uuid-keyed directory (see [`shim_dir_path`]) is never reused, so a
    /// skipped cleanup leaks rather than being overwritten by the next run at the same pid.
    /// `crate::instance_dir::sweep_stale_instance_dirs` reclaims such a directory at the *next* launch, which is a
    /// backstop for the paths no destructor can reach (SIGKILL, a hard crash) -- not a reason to
    /// skip this call, which is what keeps a normal close from leaving anything behind at all.
    /// `Drop` is kept below for every other path (an early `build_ui` failure, a future
    /// caller that owns this on the stack); this method is what the window-close path actually
    /// relies on.
    pub(crate) fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for PaneSwitch {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// The one line the shim writes, turned into a direction. Anything else is rejected rather than
/// guessed at -- the socket is only ever written to by the shim, but a garbled message must not
/// be able to move the user's focus somewhere they didn't ask for.
fn parse_direction(line: &str) -> Option<char> {
    match line.trim() {
        "L" => Some('L'),
        "R" => Some('R'),
        "U" => Some('U'),
        "D" => Some('D'),
        _ => None,
    }
}

/// The private directory holding this `PaneSwitch`'s fake-`tmux` symlink and its switch socket.
///
/// Per *instance*, not per process. `shell` builds one `PaneSwitch` per window, and a pid-keyed
/// path silently collapses two of them onto one directory the moment anything builds two in one
/// process.
///
/// **The failure that collapse produced is worth naming exactly, because it is the opposite of the
/// one it looks like.** The old pid-keyed code `remove_dir_all`'d the directory *before* creating
/// it, so a second `PaneSwitch::new()` in one process did not hit `EADDRINUSE` -- it deleted the
/// first window's socket file and `tmux` symlink and then bound its own, successfully. The window
/// that broke was the **first** one, and it broke silently: its `UnixListener` stayed bound to an
/// inode with no name left in the filesystem, so the shim could never reach it again, and its nvim
/// child's `exepath("tmux")` pointed at a symlink that was gone. Nothing logged, nothing failed,
/// and `Ctrl+h`/`Ctrl+l` simply stopped doing anything in a window that was still open.
///
/// Keying on a uuid makes that unrepresentable rather than merely unlikely, and keeps this module
/// honest independently of whether `main()` happens to pass `NON_UNIQUE` today.
///
/// The pid stays in the name for two reasons: `ls "$TMPDIR"` while a window is open still says
/// which process a directory belongs to, and it is what `crate::instance_dir::sweep_stale_instance_dirs`
/// reads to tell a leaked directory from a live one.
///
/// The trade-off is real and worth stating plainly -- a `shell` that dies without running
/// `cleanup()` or `Drop` (SIGKILL, a hard crash) **leaks** its directory, where the old pid-keyed
/// path was reclaimed by the next process to draw the same pid. `crate::instance_dir::sweep_stale_instance_dirs`
/// is what replaces that reclamation; a socket collision between two live windows had no recovery at all.
fn shim_dir_path() -> PathBuf {
    crate::instance_dir::instance_dir_path(&std::env::temp_dir(), SHIM_DIR_PREFIX)
}

/// Finds the `neovibe-tmux-shim` binary next to the currently-running executable. Both are
/// binaries of the same Cargo package, so they always land in the same directory -- `target/debug`
/// during development, a single `bin/` directory for any real install.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly_the_four_directions() {
        assert_eq!(parse_direction("R\n"), Some('R'));
        assert_eq!(parse_direction("L\n"), Some('L'));
        assert_eq!(parse_direction("U\n"), Some('U'));
        assert_eq!(parse_direction("D\n"), Some('D'));
        // Trailing/leading whitespace is tolerated; anything else is not a direction.
        assert_eq!(parse_direction("  R  "), Some('R'));
        assert_eq!(parse_direction("r\n"), None);
        assert_eq!(parse_direction(""), None);
        assert_eq!(parse_direction("RIGHT\n"), None);
    }

    #[test]
    fn each_shim_directory_is_unique_but_still_names_its_process() {
        let a = shim_dir_path();
        let b = shim_dir_path();
        // The whole point of A5: two `PaneSwitch`es built in one process get two directories, so
        // neither can bind the other's socket path or delete the other's symlink.
        assert_ne!(a, b);

        let prefix = format!("neovibe-pane-switch-{}-", std::process::id());
        for dir in [&a, &b] {
            assert!(dir.starts_with(std::env::temp_dir()), "not under TMPDIR: {}", dir.display());
            let name = dir.file_name().expect("a named directory").to_string_lossy().into_owned();
            assert!(name.starts_with(&prefix), "pid missing from {name}");
            // A hyphenated uuid, not an empty tail -- a `format!` that lost its uuid argument
            // would still satisfy the `starts_with` above while reintroducing the collision.
            assert_eq!(name.len() - prefix.len(), 36, "expected a uuid suffix in {name}");
        }
    }

    #[test]
    fn child_env_prepends_to_path_and_never_touches_this_process() {
        // Uses a `PaneSwitch` built by hand rather than `new()` so the test needs no shim binary
        // on disk; `child_env` is pure string composition over `dir`/`socket_path`.
        let dir = std::env::temp_dir().join("neovibe-pane-switch-unit-test");
        let switch = PaneSwitch {
            socket_path: dir.join("switch.sock"),
            dir: dir.clone(),
            listener: None,
        };
        let env: std::collections::HashMap<String, String> = switch.child_env().into_iter().collect();

        assert_eq!(env["TMUX_PANE"], "%0");
        // Non-empty (so `vim-tmux-navigator` takes its tmux-aware branch at all) and free of the
        // substring the plugin uses to decide it should invoke `tmate` instead.
        assert!(!env["TMUX"].is_empty());
        assert!(!env["TMUX"].contains("tmate"));
        // The socket is the first comma-separated field, which is what `s:TmuxSocket()` reads.
        assert_eq!(env["TMUX"].split(',').next().unwrap(), switch.socket_path.display().to_string());
        assert_eq!(env["NEOVIBE_PANE_SWITCH_SOCKET"], switch.socket_path.display().to_string());

        // PATH is *prepended to*, not replaced -- the child still needs to find `nvim`'s own
        // helpers, language servers, and everything else the user's config shells out to.
        let bin_dir = dir.join("bin").display().to_string();
        assert!(env["PATH"].starts_with(&format!("{bin_dir}:")), "got {}", env["PATH"]);
        if let Ok(existing) = std::env::var("PATH") {
            assert!(env["PATH"].ends_with(&existing));
        }

        // The whole point: none of this is visible to the host process itself.
        assert!(std::env::var_os("TMUX").is_none() || std::env::var("TMUX").unwrap() != env["TMUX"]);
        assert!(std::env::var_os("NEOVIBE_PANE_SWITCH_SOCKET").is_none());

        // `switch` was built by hand and owns no real directory; make sure Drop's remove_dir_all
        // can't take out anything real if this test's temp path ever happened to exist.
        std::mem::forget(switch);
    }
}
