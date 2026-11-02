//! The pane-switch *protocol*: the private directory, the fake-`tmux` symlink, the socket the shim
//! writes one direction letter to, and the parser that reads it. A second message on that socket,
//! `Q <generation>`, is sent by
//! [`crate::layout::kill::editor_quit_lua`] when a `:confirm qall` it asked nvim for is cancelled; a
//! third, `W <word>…`, carries the words of an `:Eitri` command ([`crate::nvim_verbs`] reads them).
//! `editor_watch.lua` sends the rest: `nvim-watched` and `nvim-leaving` tell an asked quit from a
//! crash, and `S <count> <name>` names the project's files with unsaved changes in swap files.
//! Toolkit-free.
//!
//! The same directory holds `nav_fallback.lua`, a `--cmd`-loaded snippet that gives `Ctrl+h/j/k/l`
//! a way out of the editor
//! when vim-tmux-navigator is absent, from Visual mode, and (`Ctrl+l` only) from Insert
//! mode, writing the same letters to the same socket as the shim. It lives here because leaving can
//! only work where the socket does.
//!
//! The half that is **not** here is the polling driver: `shell::pane_switch` owns the
//! `glib::timeout_add_local` timer that constructs a [`PaneSwitchReader`] once and calls
//! [`PaneSwitchReader::poll`] every tick, handing each message to a GTK focus grab or a
//! kill-cancellation clear. That split -- the protocol in the core, the polling driver in the
//! shell -- is what lets a second host (the macOS host) reuse every line below with its own
//! run-loop timer.
//!
//! The *why* of the mechanism as a whole -- why `shell` fakes tmux for the embedded nvim instead of
//! intercepting `Ctrl+h`/`Ctrl+l` itself, and why each direction means what it means -- stays in
//! `shell::pane_switch`'s own module doc, because it is about panes and focus, which this crate has
//! neither of.

use crate::editor_reopen::{EDITOR_REOPEN_CAPTURE_LUA, EDITOR_REOPEN_LUA, REOPEN_CAPTURE_NAME, REOPEN_LUA_NAME};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often a host should call [`PaneSwitchReader::poll`]. A keypress-driven focus switch at
/// this cadence is imperceptible, and an idle poll of a non-blocking `accept()` costs one failing
/// syscall.
///
/// It lives beside the protocol rather than beside the timer that consumes it because the
/// `#[ignore]`d real-nvim tests in [`crate::theme::feed`] poll at the same cadence to reproduce what
/// the main loop does; one number, two readers.
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bound on how large one message may grow before [`PaneSwitchReader`] drops it with a log rather
/// than retaining it without bound. A pane-switch message is a handful of bytes (`"R\n"`,
/// `"Q 4294967295\n"`); nothing legitimate comes close.
const MAX_MESSAGE_BYTES: usize = 4096;

/// Bound on how many bytes [`PaneSwitchReader::poll`] reads in one call, across every connection it
/// looks at. Since `poll` only ever reads the oldest pending connection (see its own doc), this
/// mainly bounds how much of one oversized sender's data a single poll copies before the byte cap
/// above closes it -- a little over [`MAX_MESSAGE_BYTES`] so a message at that cap is judged within
/// one poll rather than spread across several.
const MAX_READ_BYTES_PER_POLL: usize = 2 * MAX_MESSAGE_BYTES;

/// Wall-clock bound on how long one [`PaneSwitchReader::poll`] call may spend reading, independent
/// of the byte budget above. Every read `poll` performs is already non-blocking, so this is a
/// backstop against slow syscalls (e.g. under memory pressure) rather than the limiter that matters
/// day to day -- the byte budget, and the fact that a `WouldBlock` returns immediately, are.
///
/// Measured on the real clock from the start of the read loop, not from the start of `poll` (the
/// accepts before it are bounded by [`MAX_ACCEPTS_PER_POLL`] instead), and checked *after* each
/// connection's read, never before the first: a poll that runs out of time still reads its front
/// connection once, so it always makes progress.
const MAX_POLL_DURATION: Duration = Duration::from_millis(5);

/// How many new connections one [`PaneSwitchReader::poll`] call accepts. A burst of keypresses past
/// this leaves the rest queued in the kernel's own (FIFO) backlog for the next poll, one
/// [`POLL_INTERVAL`] later -- accept order is still preserved, just spread across polls.
const MAX_ACCEPTS_PER_POLL: usize = 16;

/// How long an accepted connection may sit with no complete message before [`PaneSwitchReader`]
/// drops it with a log. The shim writes immediately after `connect()` returns, so in practice this
/// is never reached for a real sender; it exists so a connection that never writes anything (a
/// broken client, or a probe) cannot block every message behind it forever -- see
/// [`PaneSwitchReader::poll`]'s own doc for why a stuck connection blocks the ones after it at all.
const MAX_PENDING_AGE: Duration = Duration::from_secs(2);

/// Hard cap on how many connections [`PaneSwitchReader`] retains at once, across every age.
/// Without this, a same-user sender that connects without ever writing (the case [`MAX_PENDING_AGE`]
/// exists for) could accumulate up to `MAX_ACCEPTS_PER_POLL * (MAX_PENDING_AGE / POLL_INTERVAL)` --
/// about 1280 -- open file descriptors before its own age evicts even the first one, which can
/// exceed a typical 1024-fd soft `RLIMIT_NOFILE` (a same-user, 0700-directory
/// sender gains nothing from this beyond running its own process out of descriptors first, but the
/// bound belongs in the type rather than only in the two numbers above). Once the cap is hit,
/// `poll` simply stops accepting for the rest of that call; a burst past it waits in the kernel's
/// own backlog the same way a burst past [`MAX_ACCEPTS_PER_POLL`] already does.
const MAX_PENDING_CONNECTIONS: usize = 64;

/// The directory-name prefix [`crate::instance_dir`] keys this module's directories on.
///
/// Short names and a compact uuid keep the path within macOS's 103-byte limit
/// ([`agent::socket_path::MAX_SOCKET_PATH_BYTES`]) under a 49-byte `TMPDIR`. The longer
/// `neovibe-pane-switch-` prefix, hyphenated uuid and `switch.sock` total 124 bytes, 21 over.
/// `SWEPT_NAMES` retains the names needed to reclaim those directories.
pub(crate) const DIR_PREFIX: &str = "nv-ps-";

/// The socket file inside that directory; kept short for the path budget described by [`DIR_PREFIX`].
const SOCKET_NAME: &str = "s.sock";

/// Every `(directory prefix, socket file)` pair this module has ever used, current first.
///
/// [`sweep_stale_dirs`] walks all of them, so a directory a pre-rename build leaked is still
/// reclaimed rather than left under `TMPDIR` forever: the sweep matches **by prefix**, so a rename
/// with no second sweep would orphan every such directory permanently, with nothing left in the
/// code that names it. A pre-rename build that is still *running* is spared by the same two checks
/// as always -- its pid is alive, and its `switch.sock` answers a `connect()`.
/// `the_sweep_still_reclaims_a_directory_under_the_old_prefix` pins that the legacy row is really swept and
/// not merely written down.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME), ("neovibe-pane-switch-", "switch.sock")];

/// The nav fallback's file name inside the channel's directory. `dofile`d through an environment
/// variable, never bound, so no `sockaddr_un` limit applies to it.
const NAV_LUA_NAME: &str = "nav_fallback.lua";

/// The nav fallback itself: a Lua copy of
/// vim-tmux-navigator's `s:TmuxAwareNavigate` for Normal mode, plus a Visual-mode one that keeps the
/// selection at nvim's edge, installed only where the global slot is empty, nvim's own default or a
/// plain `<C-W>{dir}` move.
pub(crate) const NAV_FALLBACK_LUA: &str = include_str!("nav_fallback.lua");

/// The editor watch's file name inside the channel's directory, beside the nav fallback's.
const WATCH_LUA_NAME: &str = "editor_watch.lua";

/// What tells the window that nvim is leaving on purpose, and what a start finds in swap files.
pub(crate) const EDITOR_WATCH_LUA: &str = include_str!("editor_watch.lua");

/// The editor watch's `--cmd`, guarded as [`NAV_LOADER_CMD`] is.
pub(crate) const WATCH_LOADER_CMD: &str =
    "lua local p = vim.env.EITRI_EDITOR_WATCH_LUA; if p and p ~= '' then pcall(dofile, p) end";

/// The reopen loader runs only when its snippet was written successfully.
pub(crate) const REOPEN_LOADER_CMD: &str =
    "lua local p = vim.env.EITRI_REOPEN_LUA; if p and p ~= '' then pcall(dofile, p) end";

/// The one `--cmd`, guarded the way `nvim_keys`'s loader is: the path is checked before
/// `dofile`, because `dofile(nil)` reads stdin -- under `--embed`, the RPC pipe -- and the load is
/// `pcall`ed, so a broken snippet costs the fallback, never the editor.
pub(crate) const NAV_LOADER_CMD: &str =
    "lua local p = vim.env.EITRI_NAV_LUA; if p and p ~= '' then pcall(dofile, p) end";

/// A live pane-switch channel: a private directory holding the fake-`tmux` symlink, the Unix
/// socket and the nav fallback's snippet, plus the bound listener.
///
/// Both `Drop` and the explicit [`PaneSwitchChannel::cleanup`] remove that whole directory, but
/// **neither is guaranteed to run**, so this type does not promise that nothing is left behind. In
/// `shell` the value lives inside the window's `close-request` closure, which GTK does not
/// deterministically free before process exit, so the close handler calls cleanup explicitly.
/// No destructor runs on SIGKILL or a hard crash; the dead symlink and socket survive under `TMPDIR`.
/// [`sweep_stale_dirs`] is what reclaims them on a later launch.
pub struct PaneSwitchChannel {
    dir: PathBuf,
    socket_path: PathBuf,
    /// `None` when the snippet could not be written: the channel still serves the shim, and nvim
    /// simply gets no fallback (neither `EITRI_NAV_LUA` nor the loader).
    nav_lua: Option<PathBuf>,
    /// `None` when the snippet could not be written: nvim then never says it is watched, and its exit
    /// is taken as a quit, as with no socket at all.
    watch_lua: Option<PathBuf>,
    reopen_lua: Option<PathBuf>,
    listener: Option<UnixListener>,
    /// The clipboard socket's listener ([`crate::host_clipboard`]), bound only beside the shim (an nvim
    /// with no fake `tmux` never asks for it); `None` when it could not be bound, and nvim's `+`
    /// register then reaches no clipboard, as before.
    clipboard_listener: Option<UnixListener>,
    /// Whether `bin/tmux` was linked in. Without it the child gets neither the fake `TMUX`
    /// variables nor a `PATH` that leads into a directory that does not exist.
    shim: bool,
}

/// Reclaims directories dead processes left behind, under both current and legacy names.
///
/// Separate from [`PaneSwitchChannel::bind`] so a host can run it even on a path that is about to
/// give up (`shell` cannot bind without its shim binary, and still owes the sweep).
pub fn sweep_stale_dirs() {
    let tmp = std::env::temp_dir();
    for (prefix, socket_name) in SWEPT_NAMES {
        crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "pane_switch");
    }
}

impl PaneSwitchChannel {
    /// Builds the shim directory, links `shim` in as `bin/tmux` and binds the socket, or returns
    /// `None` having logged why. `None` is a supported outcome, not an error path: the caller
    /// simply injects no environment and the embedded nvim behaves as it does outside tmux.
    ///
    /// Does **not** sweep; call [`sweep_stale_dirs`] first.
    pub fn bind(shim: &Path) -> Option<Self> {
        Self::bind_with(Some(shim))
    }

    /// The same channel without the fake `tmux`: no `bin/` directory, no symlink, and
    /// [`Self::child_env`] leaves `TMUX`, `TMUX_PANE` and `PATH` alone. For a host that attaches to
    /// an nvim it did not start and must never change that nvim's environment.
    pub fn bind_without_shim() -> Option<Self> {
        Self::bind_with(None)
    }

    fn bind_with(shim: Option<&Path>) -> Option<Self> {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        let bin_dir = dir.join("bin");
        // No `remove_dir_all` first: `instance_dir_path` returns a path that has never existed, so
        // there is nothing to clear -- and an unconditional recursive delete of a `TMPDIR`-derived
        // path this process has not yet created is worth not having at all.
        //
        // 0700, as the three sibling feeds create theirs, and `create` rather than `create_dir_all`
        // for the same reason they use it: a path that already exists is someone else's. `bin/` is
        // first on nvim's `PATH` and the nav snippet is `dofile`d by nvim, so under umask 002 with a
        // shared primary group the old `0775` let another member of that group plant an executable
        // or rewrite the Lua -- code run as this user.
        if let Err(e) = crate::instance_dir::create_private_dir(&dir) {
            eprintln!(
                "[pane_switch] could not create {}: {e} -- Ctrl+h/j/k/l out of nvim disabled",
                dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[pane_switch] {what} -- Ctrl+h/j/k/l out of nvim disabled");
            let _ = std::fs::remove_dir_all(&dir);
        };
        let fake_tmux = bin_dir.join("tmux");
        if let Some(shim) = shim {
            if let Err(e) = crate::instance_dir::create_private_dir(&bin_dir) {
                fail(format!("could not create {}: {e}", bin_dir.display()));
                return None;
            }
            if let Err(e) = std::os::unix::fs::symlink(shim, &fake_tmux) {
                fail(format!(
                    "could not link {} -> {}: {e}",
                    fake_tmux.display(),
                    shim.display()
                ));
                return None;
            }
        }

        // Through `agent::socket_path::in_dir`, which is this workspace's one place that knows the
        // 103-byte cap a `sockaddr_un` path has on macOS. A path over it is refused here, naming
        // itself and its length, rather than failing inside `bind` with std's own message.
        let socket_path = match agent::socket_path::in_dir(&dir, SOCKET_NAME) {
            Ok(path) => path,
            Err(e) => {
                fail(format!("{e}"));
                return None;
            }
        };
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                fail(format!("could not bind {}: {e}", socket_path.display()));
                return None;
            }
        };
        if let Err(e) = crate::instance_dir::make_socket_private(&socket_path) {
            fail(format!(
                "could not make {} reachable by nvim: {e}",
                socket_path.display()
            ));
            return None;
        }
        if let Err(e) = listener.set_nonblocking(true) {
            fail(format!("could not set the switch socket non-blocking: {e}"));
            return None;
        }

        let clipboard_listener = shim.and_then(|_| bind_clipboard(&dir));

        // Optional, unlike everything above: without it vim-tmux-navigator still leaves through the
        // shim, so a failed write costs only the fallback, never the channel.
        let nav_path = dir.join(NAV_LUA_NAME);
        let nav_lua = match agent::private_fs::write_private(&nav_path, NAV_FALLBACK_LUA.as_bytes()) {
            Ok(()) => Some(nav_path),
            Err(e) => {
                eprintln!(
                    "[pane_switch] could not write {}: {e} -- no Ctrl+h/j/k/l fallback without vim-tmux-navigator",
                    nav_path.display()
                );
                None
            }
        };

        let watch_path = dir.join(WATCH_LUA_NAME);
        let watch_lua = match agent::private_fs::write_private(&watch_path, EDITOR_WATCH_LUA.as_bytes()) {
            Ok(()) => Some(watch_path),
            Err(e) => {
                eprintln!(
                    "[pane_switch] could not write {}: {e} -- a crash of nvim closes the window like a quit",
                    watch_path.display()
                );
                None
            }
        };

        let mut reopen_lua = None;
        for (name, contents) in [
            (REOPEN_LUA_NAME, EDITOR_REOPEN_LUA),
            (REOPEN_CAPTURE_NAME, EDITOR_REOPEN_CAPTURE_LUA),
        ] {
            let path = dir.join(name);
            match agent::private_fs::write_private(&path, contents.as_bytes()) {
                Ok(()) => {
                    if name == REOPEN_LUA_NAME {
                        reopen_lua = Some(path);
                    }
                }
                Err(e) => eprintln!("[pane_switch] could not write {}: {e}", path.display()),
            }
        }

        if shim.is_some() {
            println!(
                "[pane_switch] fake tmux at {}, socket at {}",
                fake_tmux.display(),
                socket_path.display()
            );
        } else {
            println!("[pane_switch] no fake tmux, socket at {}", socket_path.display());
        }
        Some(Self {
            dir,
            socket_path,
            nav_lua,
            watch_lua,
            reopen_lua,
            listener: Some(listener),
            clipboard_listener,
            shim: shim.is_some(),
        })
    }

    /// The `(name, value)` pairs to hand to the embedded nvim child. **These must never be applied
    /// to the host process** -- see `shell::pane_switch`'s module doc.
    ///
    /// `TMUX`'s value follows real tmux's own `<socket>,<pid>,<session>` shape, and is what
    /// `vim-tmux-navigator`'s `s:TmuxSocket()` splits on `,` to build its `-S` argument; the shim
    /// ignores `-S` entirely, so only the "non-empty, and does not contain the substring `tmate`"
    /// part is load-bearing (that substring is what the plugin's `s:TmuxOrTmateExecutable` tests
    /// to decide whether to invoke `tmate` instead of `tmux`).
    ///
    /// `EITRI_NAV_LUA` names the nav fallback's snippet, which [`Self::nvim_args`]'s loader runs;
    /// it is absent when the snippet could not be written, and the loader is then a no-op.
    pub fn child_env(&self) -> Vec<(String, String)> {
        self.child_env_over(std::env::var("PATH"))
    }

    /// [`Self::child_env`] over the host's `PATH` as `std::env::var` read it. The environment handed on
    /// is text, so a `PATH` that is not UTF-8 cannot be put after the shim's `bin/`: put alone in its
    /// place it would hide every program the user's config runs. The child then keeps the `PATH` it
    /// inherits and gets no fake `tmux` (it could not find it), and Ctrl+h/j/k/l out of nvim go through
    /// the nav fallback, as without the shim.
    fn child_env_over(&self, host_path: Result<String, std::env::VarError>) -> Vec<(String, String)> {
        let mut env = Vec::new();
        let bin_dir = self.dir.join("bin");
        let path = match host_path {
            _ if !self.shim => None,
            Ok(existing) => Some(format!("{}:{}", bin_dir.display(), existing)),
            Err(std::env::VarError::NotPresent) => Some(bin_dir.display().to_string()),
            Err(std::env::VarError::NotUnicode(_)) => {
                eprintln!("[pane_switch] PATH is not UTF-8 -- no fake tmux for nvim; Ctrl+h/j/k/l use the fallback");
                None
            }
        };
        if let Some(path) = path {
            env.push((
                "TMUX".to_string(),
                format!("{},{},0", self.socket_path.display(), std::process::id()),
            ));
            env.push(("TMUX_PANE".to_string(), "%0".to_string()));
            env.push(("PATH".to_string(), path));
        }
        env.push((
            "EITRI_PANE_SWITCH_SOCKET".to_string(),
            self.socket_path.display().to_string(),
        ));
        if let Some(nav_lua) = &self.nav_lua {
            env.push(("EITRI_NAV_LUA".to_string(), nav_lua.display().to_string()));
        }
        if let Some(watch_lua) = &self.watch_lua {
            env.push(("EITRI_EDITOR_WATCH_LUA".to_string(), watch_lua.display().to_string()));
        }
        if let Some(reopen_lua) = &self.reopen_lua {
            env.push(("EITRI_REOPEN_LUA".to_string(), reopen_lua.display().to_string()));
        }
        env
    }

    /// The nav fallback, editor watch and reopen `--cmd` loaders, for the nvim child only, beside the
    /// theme, editor-context and nvim-keys feeds' own. Each is left out when its snippet could not be
    /// written.
    pub fn nvim_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.nav_lua.is_some() {
            args.extend(["--cmd".to_string(), NAV_LOADER_CMD.to_string()]);
        }
        if self.watch_lua.is_some() {
            args.extend(["--cmd".to_string(), WATCH_LOADER_CMD.to_string()]);
        }
        if self.reopen_lua.is_some() {
            args.extend(["--cmd".to_string(), REOPEN_LOADER_CMD.to_string()]);
        }
        args
    }

    /// Hands the bound listener to the host's polling driver. Returns `None` the second time, which
    /// is how a host detects that it wired two drivers to one channel.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Hands the clipboard socket's listener to the host, once; `None` without the shim, when it
    /// could not be bound, or the second time.
    pub fn take_clipboard_listener(&mut self) -> Option<UnixListener> {
        self.clipboard_listener.take()
    }

    /// Removes the shim directory (fake `tmux` symlink and socket alike). Idempotent, and safe to
    /// call while the nvim child is still alive -- by the time a host calls this it is shutting
    /// that child down anyway, and a missing `tmux` only means the plugin's `system()` call finds
    /// nothing to run.
    ///
    /// GTK does not deterministically free the window's `close-request` signal closure before
    /// process exit, so `Drop` alone can leave the directory behind on a normal close. The
    /// uuid-keyed directory is never reused: skipping cleanup leaks it. [`sweep_stale_dirs`]
    /// reclaims survivors at the next launch, including those left by SIGKILL or a hard crash;
    /// that backstop does not replace cleanup on a normal close. `Drop` also handles early
    /// failures and callers that own the channel on the stack.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// The private directory holding the channel's snippets and handoff files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The bound socket's path. Only the `TMUX`/`EITRI_PANE_SWITCH_SOCKET` env pairs need it in
    /// production; tests read it to connect.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Binds the clipboard socket in the channel's directory, 0600, or logs why not and answers `None`:
/// the channel itself does not depend on it.
fn bind_clipboard(dir: &Path) -> Option<UnixListener> {
    let warn = |what: String| {
        eprintln!("[pane_switch] {what} -- nvim's clipboard register reaches no clipboard without a tool");
    };
    let path = match agent::socket_path::in_dir(dir, crate::host_clipboard::SOCKET_NAME) {
        Ok(path) => path,
        Err(e) => {
            warn(format!("{e}"));
            return None;
        }
    };
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            warn(format!("could not bind {}: {e}", path.display()));
            return None;
        }
    };
    if let Err(e) = crate::instance_dir::make_socket_private(&path) {
        warn(format!("could not make {} private: {e}", path.display()));
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(listener)
}

impl Drop for PaneSwitchChannel {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// A message accepted off the socket: a pane-switch direction letter from the `vim-tmux-navigator`
/// shim, a kill's generation from a cancelled `:confirm qall`
/// ([`crate::layout::kill::editor_quit_lua`]), or the words of an `:Eitri` command, each one a
/// [`is_verb_word`], 1 to [`VERB_MAX_WORDS`] of them, for [`crate::nvim_verbs::parse_words`], or
/// what the editor watch reports, or a restore report ([`ReopenLine`]: the `B`, `O`, `N` and `A`
/// lines of the reopen feed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneMessage {
    Direction(char),
    QuitCancelled(u32),
    Verb(Vec<String>),
    Reopen(ReopenLine),
    /// nvim will say when it leaves: from here an exit with no [`Self::EditorLeaving`] is a crash.
    EditorWatched,
    /// nvim is quitting on purpose, not on a deadly signal.
    EditorLeaving,
    /// `count` project files have unsaved changes in swap files; `newest` is the latest of them.
    Recoverable {
        count: u32,
        newest: String,
    },
}

/// A restore report from the embedded editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReopenLine {
    Began {
        token: String,
    },
    Ended {
        token: String,
        opened: u32,
        gone: u32,
        failed: u32,
        squeezed: u32,
        micros: u64,
    },
    Name {
        token: String,
        gone: bool,
        name: String,
    },
    Aside {
        token: String,
        word: AsideWord,
    },
}

/// The closed set of reasons the editor can leave reopening to something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsideWord {
    Off,
    Args,
    Screen,
    ThisSession,
    MiniSessions,
    Persistence,
    AutoSession,
    Resession,
    Possession,
    NvimPossession,
    SessionManager,
    Persisted,
    Startify,
    Prosession,
}

impl AsideWord {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Args => "args",
            Self::Screen => "screen",
            Self::ThisSession => "this_session",
            Self::MiniSessions => "mini.sessions",
            Self::Persistence => "persistence",
            Self::AutoSession => "auto-session",
            Self::Resession => "resession",
            Self::Possession => "possession",
            Self::NvimPossession => "nvim-possession",
            Self::SessionManager => "session_manager",
            Self::Persisted => "persisted",
            Self::Startify => "startify",
            Self::Prosession => "prosession",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "off" => Some(Self::Off),
            "args" => Some(Self::Args),
            "screen" => Some(Self::Screen),
            "this_session" => Some(Self::ThisSession),
            "mini.sessions" => Some(Self::MiniSessions),
            "persistence" => Some(Self::Persistence),
            "auto-session" => Some(Self::AutoSession),
            "resession" => Some(Self::Resession),
            "possession" => Some(Self::Possession),
            "nvim-possession" => Some(Self::NvimPossession),
            "session_manager" => Some(Self::SessionManager),
            "persisted" => Some(Self::Persisted),
            "startify" => Some(Self::Startify),
            "prosession" => Some(Self::Prosession),
            _ => None,
        }
    }
}

/// A file name in a `S` line takes at most this many bytes: what the snippet sends at most, 120
/// characters of 4 bytes each.
const RECOVERABLE_NAME_MAX_BYTES: usize = 480;

/// At most this many words follow a `W`: an action name and its options, or a verb and its two
/// arguments, fit; a line that carries more was not written by `:Eitri`.
pub const VERB_MAX_WORDS: usize = 6;

/// A word is at most this many bytes: longer than any action name, module id or workspace name a
/// user types, short enough that a line stays small.
pub const VERB_MAX_WORD_BYTES: usize = 64;

/// A `W` line is at most this many bytes, its newline included.
pub const VERB_MAX_LINE_BYTES: usize = 512;

/// Whether `word` may travel in a `W` line: 1 to [`VERB_MAX_WORD_BYTES`] bytes of
/// `[A-Za-z0-9._:=<>#-]`. Every action name, `key=value` option, built-in module id (`terminal#2`)
/// and `lua:<id>` of a plain panel id is such a word; no quote, separator, control byte or
/// non-ASCII byte is, so nothing parsed from the line can carry text the sender did not mean as a
/// name.
pub fn is_verb_word(word: &str) -> bool {
    (1..=VERB_MAX_WORD_BYTES).contains(&word.len())
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'=' | b'<' | b'>' | b'#' | b'-'))
}

/// The direction a shim letter names: `vim-tmux-navigator`'s `select-pane -L/-R/-U/-D`. Anything
/// else is not a direction (the shim only ever sends these four).
pub fn letter_direction(letter: char) -> Option<crate::layout::Direction> {
    use crate::layout::Direction;
    match letter {
        'L' => Some(Direction::Left),
        'R' => Some(Direction::Right),
        'U' => Some(Direction::Up),
        'D' => Some(Direction::Down),
        _ => None,
    }
}

/// Reads [`PaneMessage`]s off a pane-switch socket without ever blocking the caller on a slow or
/// trickling sender (the theme feed reads its socket the same way). Construct once per channel and
/// retain across polls -- a fresh reader
/// per call loses partial connections' buffered bytes.
///
/// A blocking `read_line` with `SO_RCVTIMEO` can hold the GTK loop for a whole trickled line:
/// each successful partial read resets the timeout. Measured here at 3.2x the stated bound,
/// 160.8 ms against a 50 ms timeout (the theme feed's separate measurement is 18x on 100 ms).
/// `poll` instead keeps incomplete connections for the next call and never waits on a read.
///
/// **Delivers every message in accept order, and never lets a newer connection's message overtake
/// an older, still-incomplete one.** [`crate::line_feed::NewestLineReader`] is not reused here for
/// exactly that reason -- it is *newest-wins*, built for a feed where only the latest snapshot
/// matters and an older one may be silently superseded. A pane-switch direction is a discrete
/// keypress: dropping or reordering one would move focus, or clear a kill flag, somewhere the user
/// never asked for. `poll` only
/// ever reads its oldest pending connection; a newer one is not looked at until the older
/// completes, is closed, or ages out (see [`MAX_PENDING_AGE`]) -- so a stuck sender blocks the
/// messages behind it rather than letting them overtake it.
///
/// **Each accepted stream is made non-blocking explicitly.** `listener` is already non-blocking
/// (the host polls it from a main loop that must not stall), and Linux does *not* carry that flag
/// onto the fd `accept()` returns; leaving it blocking can hang a read. macOS copies the listener's
/// flag (`rust-lang/rust#67027`). A stream
/// whose mode cannot be set is skipped; one that grows past [`MAX_MESSAGE_BYTES`] is dropped
/// with a log. A dropped message costs one focus switch or one stuck kill flag the user can retry,
/// not a security boundary.
pub struct PaneSwitchReader {
    listener: UnixListener,
    pending: VecDeque<PendingConnection>,
    /// [`MAX_POLL_DURATION`] in production. A field rather than the constant read inline only so
    /// this module's tests can pin the budget's two edges deterministically: zero (every poll is
    /// already past its deadline, which is what a preempted poll looks like) and effectively
    /// unlimited (for a test asserting that one poll drains a burst, which the wall-clock backstop
    /// would otherwise make depend on the scheduler).
    read_time_budget: Duration,
}

struct PendingConnection {
    stream: UnixStream,
    bytes: Vec<u8>,
    accepted_at: Instant,
}

enum ReadOutcome {
    Pending,
    Complete(Vec<u8>),
    Closed,
}

impl PendingConnection {
    /// Reads whatever is available without blocking, honouring the shared per-poll byte budget.
    /// Retains a partial line across calls (in `self.bytes`); a line that would grow past
    /// [`MAX_MESSAGE_BYTES`] is dropped -- logged, never buffered without bound.
    fn read_available(&mut self, budget: &mut usize) -> ReadOutcome {
        let mut buffer = [0u8; 512];
        while *budget > 0 {
            let limit = buffer.len().min(*budget);
            match self.stream.read(&mut buffer[..limit]) {
                // EOF: whatever was buffered is the whole message, newline or not -- the sender
                // closing right after its one line is the ordinary case, not an error.
                Ok(0) => return ReadOutcome::Complete(std::mem::take(&mut self.bytes)),
                Ok(read) => {
                    *budget -= read;
                    let newline = buffer[..read].iter().position(|byte| *byte == b'\n');
                    let length = newline.unwrap_or(read);
                    if self.bytes.len() + length > MAX_MESSAGE_BYTES {
                        eprintln!("[pane_switch] dropping a message over {MAX_MESSAGE_BYTES} bytes without a newline");
                        return ReadOutcome::Closed;
                    }
                    self.bytes.extend_from_slice(&buffer[..length]);
                    if newline.is_some() {
                        return ReadOutcome::Complete(std::mem::take(&mut self.bytes));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                    return ReadOutcome::Pending;
                }
                Err(_) => return ReadOutcome::Closed,
            }
        }
        ReadOutcome::Pending
    }
}

impl PaneSwitchReader {
    /// Takes a channel's already-bound, non-blocking listener.
    pub fn new(listener: UnixListener) -> Self {
        Self {
            listener,
            pending: VecDeque::new(),
            read_time_budget: MAX_POLL_DURATION,
        }
    }

    /// Whether a connection was accepted and its message is not finished: a host must poll again
    /// (on a timer) rather than wait for the listener, which will not say so.
    pub fn holds_connections(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The listening socket, for a host that waits for it to become readable.
    pub fn listener_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.listener)
    }

    /// Every message that became complete since the last call, in accept order. Never blocks: a
    /// connection with no complete line yet is retained for the next call rather than waited on --
    /// see this type's own doc for why that is the fix, and why a stuck connection blocks the
    /// messages behind it rather than letting a later one overtake it.
    pub fn poll(&mut self) -> Vec<PaneMessage> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Vec<PaneMessage> {
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            // As [`MAX_PENDING_CONNECTIONS`]'s own doc says: stop accepting once the
            // cap is hit, rather than letting a same-user sender that never writes grow this queue
            // without bound. A connection past the cap waits in the kernel's own backlog, same as
            // one past `MAX_ACCEPTS_PER_POLL` already does.
            if self.pending.len() >= MAX_PENDING_CONNECTIONS {
                break;
            }
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!(
                            "[pane_switch] could not make an accepted connection non-blocking: {e} -- ignoring it"
                        );
                        continue;
                    }
                    self.pending.push_back(PendingConnection {
                        stream,
                        bytes: Vec::new(),
                        accepted_at: now,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                Err(e) => {
                    eprintln!("[pane_switch] accept failed: {e}");
                    break;
                }
            }
        }

        // Read the front before judging its age: a complete message must survive even when it
        // shares accepted_at with a silent connection that just aged out. Only a front still
        // Pending after the read can be evicted as never having completed a message.
        //
        // Start the read budget on the real clock here, after the independently bounded accept
        // loop, and check it after each read so even a preempted poll makes progress once.
        // `now` ages connections in tests; using it for the work budget would disable that bound
        // for a future time or exhaust it before work for a past time. Each pass removes the
        // front or stops, keeping the loop bounded.
        let mut budget = MAX_READ_BYTES_PER_POLL;
        let deadline = Instant::now() + self.read_time_budget;
        let mut messages = Vec::new();
        while budget > 0 {
            let stale = match self.pending.front() {
                Some(front) => now.duration_since(front.accepted_at) >= MAX_PENDING_AGE,
                None => break,
            };
            // Scoped so the mutable borrow of `front` ends before `pop_front` needs `&mut self.pending`.
            let outcome = self
                .pending
                .front_mut()
                .expect("checked above")
                .read_available(&mut budget);
            match outcome {
                ReadOutcome::Pending => {
                    if stale {
                        eprintln!(
                            "[pane_switch] dropping a connection that never completed a message within {MAX_PENDING_AGE:?}"
                        );
                        // Keep going (budget permitting): the connection behind this one may
                        // already have a complete message waiting, and it is not the one that was
                        // stuck.
                        self.pending.pop_front();
                    } else {
                        // Never skip ahead to a newer connection while this one is merely young
                        // and quiet: that would be exactly the reordering this type exists to rule
                        // out.
                        break;
                    }
                }
                ReadOutcome::Closed => {
                    self.pending.pop_front();
                }
                ReadOutcome::Complete(bytes) => {
                    self.pending.pop_front();
                    let text = String::from_utf8_lossy(&bytes);
                    match parse_message(&text) {
                        Some(message) => messages.push(message),
                        None => eprintln!("[pane_switch] ignoring an unrecognized message of {} bytes", text.len()),
                    }
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        messages
    }
}

/// The one line the shim, [`crate::layout::kill::editor_quit_lua`] or `:Eitri` writes, turned into
/// a [`PaneMessage`]. Anything else is rejected rather than guessed at -- every descendant of the
/// embedded nvim can write to this socket, and a garbled message must not be able to move the
/// user's focus, clear a kill nobody asked for, or run a layout action.
///
/// The quit-generation form is parsed strictly: `"Q "` followed by one or more ASCII digits and
/// nothing else, checked digit-by-digit before `parse` -- `u32::from_str` alone accepts a leading
/// `"+"` (`"Q +7"` would otherwise parse), and a byte check is what rejects a second run of digits
/// (`"Q 7 8"`), a second space (`"Q  7"`) or an out-of-range value (`"Q 4294967296"`) instead of
/// truncating or panicking.
///
/// The `W` form is stricter still ([`parse_verb_line`]): no whitespace is trimmed but one trailing
/// newline, and the line is dropped whole on the first byte out of place, so a partial or padded
/// line never runs part of what it says.
///
/// The restore reports ([`parse_reopen_line`]) are `B <token>` (began), `O <token> <opened> <gone>
/// <failed> <squeezed> <micros>` (ended), `N <token> <g|f> <name>` (a file that is gone or failed,
/// by name) and `A <token> <word>` (the editor left reopening to [`AsideWord`]). The token is 16
/// lowercase hex characters, the counts plain ASCII digits; a name keeps its spaces, as the `S`
/// form's does.
fn parse_message(line: &str) -> Option<PaneMessage> {
    if let Some(report) = parse_reopen_line(line) {
        return Some(PaneMessage::Reopen(report));
    }
    if let Some(words) = parse_verb_line(line) {
        return Some(PaneMessage::Verb(words));
    }
    let trimmed = line.trim();
    match trimmed {
        "L" => return Some(PaneMessage::Direction('L')),
        "R" => return Some(PaneMessage::Direction('R')),
        "U" => return Some(PaneMessage::Direction('U')),
        "D" => return Some(PaneMessage::Direction('D')),
        "nvim-watched" => return Some(PaneMessage::EditorWatched),
        "nvim-leaving" => return Some(PaneMessage::EditorLeaving),
        _ => {}
    }
    if let Some(rest) = trimmed.strip_prefix("S ") {
        return parse_recoverable(rest);
    }
    let rest = trimmed.strip_prefix("Q ")?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        rest.parse::<u32>().ok().map(PaneMessage::QuitCancelled)
    } else {
        None
    }
}

/// Reopen reports preserve all spacing except one optional final newline.
fn parse_reopen_line(line: &str) -> Option<ReopenLine> {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let (kind, rest) = body.split_once(' ')?;
    let (token, fields) = rest
        .split_once(' ')
        .map_or((rest, None), |(token, fields)| (token, Some(fields)));
    if token.len() != 16 || !token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let token = token.to_string();
    match (kind, fields) {
        ("B", None) => Some(ReopenLine::Began { token }),
        ("O", Some(fields)) => {
            let parts: Vec<_> = fields.split(' ').collect();
            if parts.len() != 5
                || parts
                    .iter()
                    .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            {
                return None;
            }
            Some(ReopenLine::Ended {
                token,
                opened: parts[0].parse().ok()?,
                gone: parts[1].parse().ok()?,
                failed: parts[2].parse().ok()?,
                squeezed: parts[3].parse().ok()?,
                micros: parts[4].parse().ok()?,
            })
        }
        ("N", Some(fields)) => {
            let (kind, name) = fields.split_once(' ')?;
            if !matches!(kind, "g" | "f")
                || name.is_empty()
                || name.len() > RECOVERABLE_NAME_MAX_BYTES
                || name.chars().any(char::is_control)
            {
                return None;
            }
            Some(ReopenLine::Name {
                token,
                gone: kind == "g",
                name: name.to_string(),
            })
        }
        ("A", Some(word)) => Some(ReopenLine::Aside {
            token,
            word: AsideWord::parse(word)?,
        }),
        _ => None,
    }
}

/// `<count> <name>` after a `S `: digits as the `Q` form takes them, then a name of its own line.
fn parse_recoverable(rest: &str) -> Option<PaneMessage> {
    let (count, newest) = rest.split_once(' ')?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if newest.is_empty() || newest.len() > RECOVERABLE_NAME_MAX_BYTES || newest.chars().any(char::is_control) {
        return None;
    }
    Some(PaneMessage::Recoverable {
        count: count.parse().ok().filter(|count| *count > 0)?,
        newest: newest.to_string(),
    })
}

/// `W <word>( <word>){0,5}` with at most one trailing `\n`, at most [`VERB_MAX_LINE_BYTES`] bytes
/// in all, every word an [`is_verb_word`] and every separator exactly one space. The length is
/// checked before anything is split, and every byte before any word is kept.
fn parse_verb_line(line: &str) -> Option<Vec<String>> {
    if line.len() > VERB_MAX_LINE_BYTES {
        return None;
    }
    let body = line.strip_suffix('\n').unwrap_or(line);
    let rest = body.strip_prefix("W ")?;
    let words: Vec<&str> = rest.split(' ').collect();
    if words.len() > VERB_MAX_WORDS || !words.iter().all(|word| is_verb_word(word)) {
        return None;
    }
    Some(words.into_iter().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Direction;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    #[test]
    fn a_name_keeps_a_space_at_either_end() {
        let token = "0123456789abcdef";
        for name in ["trailing ", " leading", "cut at 120 "] {
            assert_eq!(
                parse_message(&format!("N {token} g {name}")),
                Some(PaneMessage::Reopen(ReopenLine::Name {
                    token: token.into(),
                    gone: true,
                    name: name.into(),
                })),
                "{name:?}"
            );
        }
    }

    #[test]
    fn each_reopen_line_parses() {
        let token = "0123456789abcdef";
        let mut cases = vec![
            (format!("B {token}"), ReopenLine::Began { token: token.into() }),
            (
                format!("O {token} 1 2 3 4 18446744073709551615"),
                ReopenLine::Ended {
                    token: token.into(),
                    opened: 1,
                    gone: 2,
                    failed: 3,
                    squeezed: 4,
                    micros: u64::MAX,
                },
            ),
            (
                format!("N {token} g src/a b.rs"),
                ReopenLine::Name {
                    token: token.into(),
                    gone: true,
                    name: "src/a b.rs".into(),
                },
            ),
            (
                format!("N {token} f {}", "é".repeat(240)),
                ReopenLine::Name {
                    token: token.into(),
                    gone: false,
                    name: "é".repeat(240),
                },
            ),
        ];
        for (word, aside) in [
            ("off", AsideWord::Off),
            ("args", AsideWord::Args),
            ("screen", AsideWord::Screen),
            ("this_session", AsideWord::ThisSession),
            ("mini.sessions", AsideWord::MiniSessions),
            ("persistence", AsideWord::Persistence),
            ("auto-session", AsideWord::AutoSession),
            ("resession", AsideWord::Resession),
            ("possession", AsideWord::Possession),
            ("nvim-possession", AsideWord::NvimPossession),
            ("session_manager", AsideWord::SessionManager),
            ("persisted", AsideWord::Persisted),
            ("startify", AsideWord::Startify),
            ("prosession", AsideWord::Prosession),
        ] {
            assert_eq!(aside.as_str(), word);
            cases.push((
                format!("A {token} {word}"),
                ReopenLine::Aside {
                    token: token.into(),
                    word: aside,
                },
            ));
        }
        for (line, expected) in cases {
            for suffix in ["", "\n"] {
                assert_eq!(
                    parse_message(&format!("{line}{suffix}")),
                    Some(PaneMessage::Reopen(expected.clone())),
                    "{line:?}"
                );
            }
        }
    }

    #[test]
    fn bad_reopen_lines_are_dropped() {
        let token = "0123456789abcdef";
        let mut bad = vec![
            "B".into(),
            format!("B  {token}"),
            format!("B {token} "),
            format!(" B {token}"),
            "B 0123456789abcde".into(),
            "B 0123456789abcdeF".into(),
            format!("B {token}\n\n"),
            format!("O {token} 1 2 3 4"),
            format!("O {token} +1 2 3 4 5"),
            format!("O {token} 1 2 3 4 -5"),
            format!("O {token} 4294967296 2 3 4 5"),
            format!("O {token} 1 2 3 4 18446744073709551616"),
            format!("N {token} x name"),
            format!("N {token} g "),
            format!("N {token} g a\tb"),
            format!("N {token} g {}", "a".repeat(481)),
            format!("A {token} unknown"),
        ];
        for line in [
            format!("O {token} 1 2 3 4 5"),
            format!("N {token} g name"),
            format!("A {token} off"),
        ] {
            bad.extend([format!(" {line}"), line.replacen(' ', "  ", 1)]);
            // A space after an `N` line's name is part of the name, as in the `S` form.
            if !line.starts_with("N ") {
                bad.push(format!("{line} "));
            }
        }
        for line in bad {
            for suffix in ["", "\n"] {
                assert_eq!(parse_message(&format!("{line}{suffix}")), None, "{line:?}");
            }
        }
    }

    #[test]
    fn bind_writes_the_reopen_snippets_beside_the_watch() {
        use crate::editor_reopen::*;
        use std::os::unix::fs::PermissionsExt;
        let channel = PaneSwitchChannel::bind_without_shim().expect("bind");
        assert_eq!(channel.dir(), channel.socket_path().parent().unwrap());
        assert!(channel.dir().join(WATCH_LUA_NAME).is_file());
        for (name, contents) in [
            (REOPEN_LUA_NAME, EDITOR_REOPEN_LUA),
            (REOPEN_CAPTURE_NAME, EDITOR_REOPEN_CAPTURE_LUA),
        ] {
            let path = channel.dir().join(name);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let env: std::collections::HashMap<_, _> = channel.child_env().into_iter().collect();
        assert_eq!(
            env["EITRI_REOPEN_LUA"],
            channel.dir().join(REOPEN_LUA_NAME).display().to_string()
        );
    }

    #[test]
    fn the_shims_four_letters_are_the_four_directions() {
        assert_eq!(letter_direction('L'), Some(Direction::Left));
        assert_eq!(letter_direction('R'), Some(Direction::Right));
        assert_eq!(letter_direction('U'), Some(Direction::Up));
        assert_eq!(letter_direction('D'), Some(Direction::Down));
        assert_eq!(letter_direction('r'), None);
        assert_eq!(letter_direction('X'), None);
    }

    /// A generous budget isolates whether one poll handles several connections from scheduler stalls.
    /// [`MAX_POLL_DURATION`] is a wall-clock backstop, so under it such an assertion depends on the
    /// scheduler: a stall of 5ms between two reads legitimately leaves the rest for the next poll.
    /// The budget's own behaviour is pinned separately, at zero, by
    /// `a_poll_past_its_time_budget_still_delivers_its_front_message_and_keeps_the_rest_in_order`.
    const UNHURRIED: Duration = Duration::from_secs(60);

    /// A hang guard keeps a regression in `set_nonblocking(true)` from hanging the test process:
    /// without it, setting an accepted stream back to blocking makes several of the "must never
    /// block" tests below run past 60s rather than fail (checked by making that very change).
    /// Run `poll` on its own thread and receive its reader and result through a channel. If a syscall
    /// blocks, the timeout abandons that thread rather than joining it; the thread touches only
    /// this test's socket and directory and dies when the test binary exits. The timeout guards
    /// against a hang, while the returned messages establish the poll's behavior.
    fn poll_within(
        mut reader: PaneSwitchReader,
        timeout: Duration,
        at: Option<Instant>,
    ) -> (PaneSwitchReader, Vec<PaneMessage>) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let messages = match at {
                Some(now) => reader.poll_at(now),
                None => reader.poll(),
            };
            let _ = tx.send((reader, messages));
        });
        rx.recv_timeout(timeout).unwrap_or_else(|_| {
            panic!("poll() did not return within {timeout:?} -- looks like a blocking-read regression, not a slow poll")
        })
    }

    #[test]
    fn parses_exactly_the_four_directions_and_a_quit_generation() {
        assert_eq!(parse_message("R\n"), Some(PaneMessage::Direction('R')));
        assert_eq!(parse_message("L\n"), Some(PaneMessage::Direction('L')));
        assert_eq!(parse_message("U\n"), Some(PaneMessage::Direction('U')));
        assert_eq!(parse_message("D\n"), Some(PaneMessage::Direction('D')));
        // Trailing/leading whitespace is tolerated; anything else is not a direction.
        assert_eq!(parse_message("  R  "), Some(PaneMessage::Direction('R')));
        assert_eq!(parse_message("r\n"), None);
        assert_eq!(parse_message(""), None);
        assert_eq!(parse_message("RIGHT\n"), None);

        assert_eq!(parse_message("Q 7\n"), Some(PaneMessage::QuitCancelled(7)));
        assert_eq!(parse_message("  Q 0  "), Some(PaneMessage::QuitCancelled(0)));
        assert_eq!(
            parse_message("Q 4294967295"),
            Some(PaneMessage::QuitCancelled(u32::MAX))
        );
        for bad in [
            "Q",
            "Q x",
            "Q -1",
            "QQ 1",
            "Q +7",
            "Q  7",
            "Q 7 8",
            "q 7",
            "Q 4294967296",
        ] {
            assert_eq!(parse_message(bad), None, "{bad:?} must not parse");
        }
    }

    fn verb(words: &[&str]) -> Option<PaneMessage> {
        Some(PaneMessage::Verb(words.iter().map(|w| w.to_string()).collect()))
    }

    #[test]
    fn a_w_line_of_plain_words_is_a_verb() {
        assert_eq!(parse_message("W zoom\n"), verb(&["zoom"]));
        assert_eq!(
            parse_message("W zoom"),
            verb(&["zoom"]),
            "a sender that closes without a newline"
        );
        assert_eq!(parse_message("W focus terminal#2\n"), verb(&["focus", "terminal#2"]));
        assert_eq!(
            parse_message("W resize.left cells=5\n"),
            verb(&["resize.left", "cells=5"])
        );
        assert_eq!(
            parse_message("W split right lua:notes\n"),
            verb(&["split", "right", "lua:notes"])
        );
        let six = "W a b c d e f\n";
        assert_eq!(parse_message(six), verb(&["a", "b", "c", "d", "e", "f"]));
        let longest = format!("W {}\n", "a".repeat(VERB_MAX_WORD_BYTES));
        assert_eq!(parse_message(&longest), verb(&[&"a".repeat(VERB_MAX_WORD_BYTES)]));
        for byte in "ABCxyz0189._:=<>#-".chars() {
            let line = format!("W {byte}\n");
            assert_eq!(parse_message(&line), verb(&[&byte.to_string()]), "{line:?}");
        }
    }

    #[test]
    fn a_w_line_with_any_byte_out_of_place_is_dropped_whole() {
        let long_word = format!("W {}", "a".repeat(VERB_MAX_WORD_BYTES + 1));
        let long_line = format!("W {}", "a".repeat(VERB_MAX_LINE_BYTES + 1 - 2));
        assert_eq!(long_line.len(), VERB_MAX_LINE_BYTES + 1);
        for bad in [
            "W",
            "W\n",
            "W ",
            "W \n",
            "W  zoom",
            "W zoom ",
            "W zoom\n\n",
            " W zoom",
            "W zoom a b c d e f",
            long_word.as_str(),
            long_line.as_str(),
            "W zoom;",
            "W zo;om",
            "W \"zoom\"",
            "W zo\"om",
            "W zoom x\t",
            "W zoom\tx",
            "W\tzoom",
            "W zo\0om",
            "W zo\u{e9}om",
            "W zo\u{fffd}om",
            "W zoom\r\n",
            "W zo/om",
            "W zo'om",
            "W zo$om",
            "W zo|om",
            "W zo,om",
            "w zoom",
            "WW zoom",
            "W:zoom",
        ] {
            assert_eq!(parse_message(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_seven_word_line_is_refused_though_each_word_is_valid() {
        assert_eq!(parse_message("W a b c d e f g\n"), None);
    }

    #[test]
    fn each_shim_directory_is_unique_but_still_names_its_process() {
        let tmp = std::env::temp_dir();
        let a = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        let b = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // The point of a directory per channel: two channels built in one process get two directories, so neither
        // can bind the other's socket path or delete the other's symlink.
        assert_ne!(a, b);

        let prefix = format!("{DIR_PREFIX}{}-", std::process::id());
        for dir in [&a, &b] {
            assert!(dir.starts_with(&tmp), "not under TMPDIR: {}", dir.display());
            let name = dir
                .file_name()
                .expect("a named directory")
                .to_string_lossy()
                .into_owned();
            assert!(name.starts_with(&prefix), "pid missing from {name}");
            // A 32-hex simple uuid, not an empty tail -- a `format!` that lost its uuid argument
            // would still satisfy the `starts_with` above while reintroducing the collision.
            assert_eq!(name.len() - prefix.len(), 32, "expected a simple-uuid suffix in {name}");
        }
    }

    #[test]
    fn child_env_prepends_to_path_and_never_touches_this_process() {
        // Built by hand rather than through `bind()` so the test needs no shim binary on disk;
        // `child_env` is pure string composition over `dir`/`socket_path`.
        let dir = std::env::temp_dir().join("nv-ps-unit-test");
        let channel = PaneSwitchChannel {
            socket_path: dir.join(SOCKET_NAME),
            nav_lua: Some(dir.join(NAV_LUA_NAME)),
            watch_lua: Some(dir.join(WATCH_LUA_NAME)),
            reopen_lua: Some(dir.join(REOPEN_LUA_NAME)),
            dir: dir.clone(),
            listener: None,
            clipboard_listener: None,
            shim: true,
        };
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();
        assert_eq!(env["EITRI_NAV_LUA"], dir.join(NAV_LUA_NAME).display().to_string());
        assert!(std::env::var_os("EITRI_NAV_LUA").is_none());

        assert_eq!(env["TMUX_PANE"], "%0");
        // Non-empty (so `vim-tmux-navigator` takes its tmux-aware branch at all) and free of the
        // substring the plugin uses to decide it should invoke `tmate` instead.
        assert!(!env["TMUX"].is_empty());
        assert!(!env["TMUX"].contains("tmate"));
        // The socket is the first comma-separated field, which is what `s:TmuxSocket()` reads.
        assert_eq!(
            env["TMUX"].split(',').next().unwrap(),
            channel.socket_path.display().to_string()
        );
        assert_eq!(
            env["EITRI_PANE_SWITCH_SOCKET"],
            channel.socket_path.display().to_string()
        );

        // PATH is *prepended to*, not replaced -- the child still needs to find `nvim`'s own
        // helpers, language servers, and everything else the user's config shells out to.
        let bin_dir = dir.join("bin").display().to_string();
        assert!(env["PATH"].starts_with(&format!("{bin_dir}:")), "got {}", env["PATH"]);
        if let Ok(existing) = std::env::var("PATH") {
            assert!(env["PATH"].ends_with(&existing));
        }

        // The whole point: none of this is visible to the host process itself.
        assert!(std::env::var_os("TMUX").is_none() || std::env::var("TMUX").unwrap() != env["TMUX"]);
        assert!(std::env::var_os("EITRI_PANE_SWITCH_SOCKET").is_none());

        // `channel` was built by hand and owns no real directory; make sure Drop's remove_dir_all
        // can't take out anything real if this test's temp path ever happened to exist.
        std::mem::forget(channel);
    }

    /// A channel whose snippet could not be written still serves the shim, and hands nvim neither
    /// half of the fallback -- a loader with no `EITRI_NAV_LUA` would be a no-op anyway, but an
    /// argument that does nothing is one more thing to misread in `ps`.
    #[test]
    fn without_its_snippet_the_channel_hands_nvim_no_fallback() {
        let dir = std::env::temp_dir().join("nv-ps-unit-test-no-nav");
        let channel = PaneSwitchChannel {
            socket_path: dir.join(SOCKET_NAME),
            nav_lua: None,
            watch_lua: None,
            reopen_lua: None,
            dir: dir.clone(),
            listener: None,
            clipboard_listener: None,
            shim: true,
        };
        assert!(channel.child_env().iter().all(|(k, _)| k != "EITRI_NAV_LUA"));
        assert!(channel.child_env().iter().all(|(k, _)| k != "EITRI_REOPEN_LUA"));
        assert!(channel.nvim_args().is_empty());
        std::mem::forget(channel);
    }

    /// A host `PATH` that is not UTF-8 is never replaced by the shim's `bin/` alone, which would hide every
    /// program nvim's config runs: the child keeps its own `PATH` and gets no fake `tmux` with it. An unset
    /// `PATH` still gets the shim's `bin/`.
    #[test]
    fn a_path_that_is_not_utf8_is_left_to_the_child() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join("nv-ps-unit-test-path");
        let channel = PaneSwitchChannel {
            socket_path: dir.join(SOCKET_NAME),
            nav_lua: Some(dir.join(NAV_LUA_NAME)),
            watch_lua: Some(dir.join(WATCH_LUA_NAME)),
            reopen_lua: Some(dir.join(REOPEN_LUA_NAME)),
            dir: dir.clone(),
            listener: None,
            clipboard_listener: None,
            shim: true,
        };
        let odd = std::ffi::OsString::from_vec(b"/usr/bin:/opt/caf\xe9/bin".to_vec());
        let env: std::collections::HashMap<String, String> = channel
            .child_env_over(Err(std::env::VarError::NotUnicode(odd)))
            .into_iter()
            .collect();
        for name in ["TMUX", "TMUX_PANE", "PATH"] {
            assert!(!env.contains_key(name), "{name} must not be set: {env:?}");
        }
        assert!(env.contains_key("EITRI_PANE_SWITCH_SOCKET") && env.contains_key("EITRI_NAV_LUA"));
        let unset: std::collections::HashMap<String, String> = channel
            .child_env_over(Err(std::env::VarError::NotPresent))
            .into_iter()
            .collect();
        assert_eq!(unset["PATH"], dir.join("bin").display().to_string());
        assert!(unset.contains_key("TMUX"));
        std::mem::forget(channel);
    }

    /// The companion's channel: no `bin/` directory, no symlink, and a child environment that
    /// carries only the socket and the snippet -- never a fake `TMUX`, `TMUX_PANE` or `PATH`.
    #[test]
    fn a_channel_without_the_shim_has_no_bin_dir_and_no_tmux_env() {
        let channel = PaneSwitchChannel::bind_without_shim().expect("bind");
        let dir = channel.socket_path().parent().unwrap().to_path_buf();
        assert!(!dir.join("bin").exists(), "no bin/ without a shim");
        assert!(dir.join(NAV_LUA_NAME).exists(), "the snippet is still written");
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();
        for name in ["TMUX", "TMUX_PANE", "PATH"] {
            assert!(!env.contains_key(name), "{name} must not be set: {env:?}");
        }
        assert_eq!(
            env["EITRI_PANE_SWITCH_SOCKET"],
            channel.socket_path().display().to_string()
        );
        assert_eq!(env["EITRI_NAV_LUA"], dir.join(NAV_LUA_NAME).display().to_string());
        channel.cleanup();
        assert!(!dir.exists());
    }

    /// `bind` writes the snippet into the channel's own directory, names it to the child, and
    /// hands nvim the loader -- and `cleanup` takes it away with the rest of the directory.
    #[test]
    fn the_editor_watchs_lines_are_read_strictly() {
        assert_eq!(parse_message("nvim-watched\n"), Some(PaneMessage::EditorWatched));
        assert_eq!(parse_message("nvim-leaving\n"), Some(PaneMessage::EditorLeaving));
        assert_eq!(
            parse_message("S 2 src/a b.txt\n"),
            Some(PaneMessage::Recoverable {
                count: 2,
                newest: "src/a b.txt".to_string()
            })
        );
        let long = "n".repeat(RECOVERABLE_NAME_MAX_BYTES + 1);
        for line in [
            "S 2",
            "S  a",
            "S x a",
            "S +2 a",
            "S 0 a",
            "S 4294967296 a",
            "S 1 a\tb",
            "nvim-leaving now",
            &format!("S 1 {long}"),
        ] {
            assert_eq!(parse_message(line), None, "{line:?}");
        }
    }

    #[test]
    fn bind_writes_the_nav_fallback_beside_the_socket() {
        // The shim is only a symlink's target here; nothing runs it.
        let channel = PaneSwitchChannel::bind(Path::new("/nonexistent/eitri-tmux-shim")).expect("bind");
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();
        let nav = PathBuf::from(&env["EITRI_NAV_LUA"]);
        assert_eq!(nav.parent(), channel.socket_path().parent());
        assert_eq!(nav.file_name().unwrap(), NAV_LUA_NAME);
        assert_eq!(std::fs::read_to_string(&nav).unwrap(), NAV_FALLBACK_LUA);
        assert_eq!(
            channel.nvim_args(),
            [
                "--cmd",
                NAV_LOADER_CMD,
                "--cmd",
                WATCH_LOADER_CMD,
                "--cmd",
                REOPEN_LOADER_CMD
            ]
            .map(str::to_string)
        );
        channel.cleanup();
        assert!(!nav.exists());
    }

    /// The instance directory and `bin/`, first on nvim's PATH, must be 0700 and the `dofile`d
    /// snippet 0600, independently of umask. Defaults such as 0775/0664 or 0755/0644 would expose
    /// code nvim executes to other users.
    #[test]
    fn the_shim_directory_and_the_nav_snippet_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let channel = PaneSwitchChannel::bind(Path::new("/nonexistent/eitri-tmux-shim")).expect("bind");
        let dir = channel.socket_path().parent().unwrap().to_path_buf();
        assert_eq!(mode(&dir), 0o700, "the instance directory");
        assert_eq!(mode(&dir.join("bin")), 0o700, "bin/, first on nvim's PATH");
        assert_eq!(mode(&dir.join(NAV_LUA_NAME)), 0o600, "the snippet nvim runs");
        channel.cleanup();
    }

    #[test]
    fn the_nav_loader_checks_its_path_before_dofile() {
        assert!(NAV_LOADER_CMD.starts_with("lua "));
        assert!(NAV_LOADER_CMD.contains("vim.env.EITRI_NAV_LUA"));
        assert!(NAV_LOADER_CMD.contains("if p and p ~= ''"));
        assert!(NAV_LOADER_CMD.contains("pcall(dofile, p)"));
    }

    /// The snippet and this module are one protocol in two languages: the letters it writes are the
    /// ones [`parse_message`] accepts, to the socket the shim writes to. The real-nvim test
    /// (`core/tests/nav_fallback_with_real_nvim.rs`) checks what it does with them.
    #[test]
    fn the_nav_snippet_writes_what_the_socket_reads() {
        for (dir, letter) in [("h", 'L'), ("j", 'D'), ("k", 'U'), ("l", 'R')] {
            let needle = format!("dir = \"{dir}\", letter = \"{letter}\"");
            assert!(NAV_FALLBACK_LUA.contains(&needle), "missing {needle:?}");
            assert_eq!(
                parse_message(&format!("{letter}\n")),
                Some(PaneMessage::Direction(letter))
            );
        }
        for needle in [
            "vim.env.EITRI_PANE_SWITCH_SOCKET",
            "letter .. \"\\n\"",
            "eitri: window or pane ",
            "\"VimEnter\"",
            "\"VeryLazy\", \"LazyLoad\"",
            "vim.schedule(",
            "nvim_get_keymap(",
        ] {
            assert!(NAV_FALLBACK_LUA.contains(needle), "missing {needle:?}");
        }
    }

    /// The seam between this crate and the host: everything a driver needs per tick is one call,
    /// and it drains a burst rather than one letter per poll.
    #[test]
    fn every_pending_connection_is_drained_in_one_pass() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);
        reader.read_time_budget = UNHURRIED;

        for letter in ["R", "D", "nonsense", "Q 3"] {
            let mut stream = UnixStream::connect(&socket_path).expect("connect");
            writeln!(stream, "{letter}").unwrap();
        }
        // A garbled message is dropped, and never stops the letters or the quit generation around it
        // being delivered, in accept order.
        assert_eq!(
            reader.poll(),
            vec![
                PaneMessage::Direction('R'),
                PaneMessage::Direction('D'),
                PaneMessage::QuitCancelled(3),
            ]
        );
        // Nothing pending: the non-blocking `accept()` returns `WouldBlock` and this returns empty
        // rather than parking the caller's main loop.
        assert!(reader.poll().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A poll must make progress even if accepts or preemption have exhausted its time budget
    /// before the first read. Checking the deadline first could return no message despite a ready
    /// front connection, and under parallel load that made this module's own suite fail `left: []`
    /// in about one run in ten. A zero budget reproduces that deterministically: each poll still delivers
    /// one front message and keeps the remaining connections in accept order.
    #[test]
    fn a_poll_past_its_time_budget_still_delivers_its_front_message_and_keeps_the_rest_in_order() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);
        reader.read_time_budget = Duration::ZERO;

        for letter in ["R", "D", "Q 3"] {
            let mut stream = UnixStream::connect(&socket_path).expect("connect");
            writeln!(stream, "{letter}").unwrap();
        }
        assert_eq!(reader.poll(), vec![PaneMessage::Direction('R')]);
        assert_eq!(reader.poll(), vec![PaneMessage::Direction('D')]);
        assert_eq!(reader.poll(), vec![PaneMessage::QuitCancelled(3)]);
        assert!(reader.poll().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A blocking `read_line` with `SO_RCVTIMEO` can hold the GTK loop for a whole trickled line:
    /// each successful partial read resets the timeout. `PaneSwitchReader::poll` instead uses
    /// nonblocking reads and retains incomplete connections.
    ///
    /// The sender writes most of the line, then waits on a channel the test holds, so the line stays
    /// incomplete for exactly as long as the test wants and no measured duration decides the outcome.
    /// A poll made while the sender is held must come back empty; once the sender finishes the line,
    /// later polls must deliver the message. The only clocks are hang guards far above any scheduler
    /// delay.
    #[test]
    fn a_trickled_message_never_holds_one_poll_past_its_budget() {
        use std::sync::mpsc::channel;

        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let reader = PaneSwitchReader::new(listener);

        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        let (partial_sent_tx, partial_sent_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let writer = std::thread::spawn(move || {
            // Leading spaces are trimmed by `parse_message`, so this still parses as `Direction('R')`.
            for byte in b"      R" {
                stream.write_all(&[*byte]).unwrap();
            }
            partial_sent_tx.send(()).unwrap();
            // Hold the newline until the test has polled the incomplete line.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            stream.write_all(b"\n").unwrap();
        });

        partial_sent_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the sender wrote the partial line");

        // The partial line is already in the socket, and the sender is held: a poll that waited for the
        // rest of the line would not return until the hang guard fires.
        let (reader, held) = poll_within(reader, Duration::from_secs(10), None);
        assert!(held.is_empty(), "an incomplete line must not be delivered: {held:?}");
        let (mut reader, held) = poll_within(reader, Duration::from_secs(10), None);
        assert!(held.is_empty(), "a second poll on the incomplete line: {held:?}");

        release_tx.send(()).unwrap();
        writer.join().unwrap();

        let mut messages = Vec::new();
        for _ in 0..1000 {
            messages.extend(reader.poll());
            if !messages.is_empty() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        assert_eq!(messages, vec![PaneMessage::Direction('R')]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every accepted stream must explicitly become nonblocking. Linux does not inherit the
    /// listener's nonblocking flag on `accept`, unlike macOS (`rust-lang/rust#67027`). Omitting
    /// `set_nonblocking(true)` would therefore hang a Linux poll on an accepted connection that
    /// has not written anything. This test exercises exactly that empty connection.
    #[test]
    fn a_connection_with_nothing_written_yet_never_blocks_a_poll() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let reader = PaneSwitchReader::new(listener);

        let mut stream = UnixStream::connect(&socket_path).expect("connect");

        // A blocking accepted stream never returns here: the only clock is a hang guard far above any scheduler delay.
        let (mut reader, empty) = poll_within(reader, Duration::from_secs(10), None);
        assert!(empty.is_empty(), "nothing written yet");

        stream.write_all(b"R\n").unwrap();
        assert_eq!(reader.poll(), vec![PaneMessage::Direction('R')]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older incomplete connection must not let a newer message overtake it; completion
    /// preserves connection order without blocking on the older sender. Reordered keystrokes
    /// could otherwise move focus somewhere the user did not intend.
    #[test]
    fn an_older_incomplete_connection_blocks_a_newer_complete_one_from_overtaking_it() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);
        reader.read_time_budget = UNHURRIED;

        // Accepted first, written last.
        let mut older = UnixStream::connect(&socket_path).expect("connect (older)");
        // Accepted second, but its whole line is already there.
        let mut newer = UnixStream::connect(&socket_path).expect("connect (newer)");
        newer.write_all(b"D\n").unwrap();

        // `older` has nothing written yet at this point -- the call that would hang under a
        // blocking-read regression, so it is the one worth the timeout guard.
        let (mut reader, empty) = poll_within(reader, Duration::from_secs(2), None);
        assert!(
            empty.is_empty(),
            "the newer, complete connection must not be delivered while the older one is still open"
        );

        older.write_all(b"L\n").unwrap();
        assert_eq!(
            reader.poll(),
            vec![PaneMessage::Direction('L'), PaneMessage::Direction('D')],
            "once the older connection completes, both arrive in accept order"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A message that never gets its newline and grows past [`MAX_MESSAGE_BYTES`] is dropped --
    /// logged, never retained without bound -- and does not stop a later, well-formed message from
    /// being delivered.
    #[test]
    fn an_oversized_message_is_dropped_without_blocking_a_later_one() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let reader = PaneSwitchReader::new(listener);

        let mut oversized = UnixStream::connect(&socket_path).expect("connect");
        // No newline: exactly at the cap is still retained... and this is the read that, under a
        // blocking-read regression, would hang forever waiting for a 9th read past the 8 that fill
        // exactly MAX_MESSAGE_BYTES (no more data is available yet, and the sender has not closed).
        oversized.write_all(&[b'x'; MAX_MESSAGE_BYTES]).unwrap();
        let (mut reader, empty) = poll_within(reader, Duration::from_secs(2), None);
        assert!(empty.is_empty());
        // ...one more byte tips it over, and it is closed rather than grown further. Data is already
        // waiting by the time this call happens, so it cannot hang even under the same regression.
        oversized.write_all(b"x").unwrap();
        assert!(reader.poll().is_empty());

        let mut good = UnixStream::connect(&socket_path).expect("connect");
        good.write_all(b"U\n").unwrap();
        assert_eq!(reader.poll(), vec![PaneMessage::Direction('U')]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A silent connection and a complete connection accepted in the same poll share `accepted_at`.
    /// Aging out the silent front must not discard its ready sibling by applying the same cutoff
    /// before reading it. The complete message must survive even though it waited behind a
    /// connection that never sent anything. A `poll_at` that re-checked the new front against the
    /// cutoff unread lost the ready sibling's `D`, and its log line said "never completed a message"
    /// although it had.
    #[test]
    fn a_stale_connection_never_drops_a_ready_sibling_accepted_with_it() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);
        // The second poll below must both evict the silent front and deliver its sibling; see
        // `UNHURRIED` for why that assertion needs the wall-clock backstop out of the way.
        reader.read_time_budget = UNHURRIED;

        // Accept both connections in one poll so they share `accepted_at`: the silent connection
        // must precede the one whose complete message is already written.
        let _silent = UnixStream::connect(&socket_path).expect("connect (silent)");
        let mut ready = UnixStream::connect(&socket_path).expect("connect (ready)");
        ready.write_all(b"D\n").unwrap();

        let now = Instant::now();
        // First poll: the silent front is `Pending` and not yet stale, so the loop stops there --
        // the ready sibling is never looked at yet, by design (never skip ahead of a young front).
        // Both polls read a connection that never writes, so both go through `poll_within`: under
        // a blocking-read regression they would otherwise hang the test binary.
        let (reader, empty) = poll_within(reader, Duration::from_secs(2), Some(now));
        assert!(empty.is_empty());

        // Age both connections past the bound and poll again: the silent front is now stale, but a
        // read is still attempted on it first (returns `Pending` again -- it really never wrote
        // anything), so it is evicted for the right reason. The loop then continues to its sibling,
        // which is *also* past the age cutoff by now but was never actually stuck -- its message
        // must still be delivered rather than discarded by the same sweep.
        let (reader, messages) = poll_within(reader, Duration::from_secs(2), Some(now + MAX_PENDING_AGE));
        assert_eq!(
            messages,
            vec![PaneMessage::Direction('D')],
            "the ready sibling's already-complete message must not be lost to the silent connection's eviction"
        );
        // And the genuinely stuck connection really was reclaimed, not merely spared by accident --
        // the eviction this type exists to do must still happen, just not to the wrong connection.
        assert!(
            reader.pending.is_empty(),
            "the truly silent connection should have been evicted, leaving nothing pending"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `MAX_PENDING_CONNECTIONS` bounds descriptors queued behind a stuck sender before its age
    /// eviction. Connect one batch per poll, totaling `MAX_PENDING_CONNECTIONS + MAX_ACCEPTS_PER_POLL`
    /// clients: that exceeds the cap while limiting the kernel listen backlog to one batch.
    /// Linux blocking Unix-socket connect waits for backlog space without a std-socket timeout,
    /// so connecting a whole flood first could hang on a host with a smaller `somaxconn`.
    /// Assert actual accepted occupancy, since an upper-bound-only check also passes an empty queue.
    #[test]
    fn pending_connections_are_capped_so_a_silent_flood_cannot_grow_without_bound() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);

        // Kept open, deliberately, and never written to: a dropped `UnixStream` would close, and
        // the reader would read its EOF and let it go rather than hold it as a silent connection.
        let mut silent_clients = Vec::new();
        let batches = MAX_PENDING_CONNECTIONS / MAX_ACCEPTS_PER_POLL + 1;
        for _ in 0..batches {
            for _ in 0..MAX_ACCEPTS_PER_POLL {
                silent_clients.push(UnixStream::connect(&socket_path).expect("connect"));
            }
            // The front never writes, so under a blocking-read regression this would hang.
            let (polled, messages) = poll_within(reader, Duration::from_secs(2), None);
            reader = polled;
            assert!(messages.is_empty(), "nothing was ever written");
        }
        assert_eq!(silent_clients.len(), MAX_PENDING_CONNECTIONS + MAX_ACCEPTS_PER_POLL);
        // Exactly the cap: every batch up to it was accepted (so the cap was really reached), and
        // the last batch was not (so it really held).
        assert_eq!(reader.pending.len(), MAX_PENDING_CONNECTIONS);

        // And it keeps holding: a poll at the cap accepts nothing, leaving that batch in the backlog.
        let (reader, messages) = poll_within(reader, Duration::from_secs(2), None);
        assert!(messages.is_empty());
        assert_eq!(reader.pending.len(), MAX_PENDING_CONNECTIONS);

        drop(silent_clients);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trap a prefix rename sets: [`sweep_stale_instance_dirs`] finds candidates **by prefix**,
    /// so a directory written under the old prefix stops being a candidate the moment the prefix
    /// changes -- it would sit under `TMPDIR` forever, holding a dead socket and a dead symlink,
    /// with nothing left in the code that names it. Sweeping [`SWEPT_NAMES`] rather than one pair
    /// is what closes that, and this asserts it against a real fixture of both shapes.
    ///
    /// [`sweep_stale_instance_dirs`]: crate::instance_dir::sweep_stale_instance_dirs
    #[test]
    fn the_sweep_still_reclaims_a_directory_under_the_old_prefix() {
        // Own root, so this never touches a real window's directory and never races the sibling
        // tests that plant fixtures of their own.
        let root = std::env::temp_dir().join(format!("nv-ps-sweeptest-{}", uuid::Uuid::new_v4().simple()));
        // pid 0 is rejected before any syscall by `pid_is_alive`, and `std::process::id()` never
        // returns it, so it is always a safe "definitely dead" pid to plant a fixture at.
        let current = root.join(format!("{DIR_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        // Exactly what a build before the prefix rename wrote: the old prefix and a hyphenated uuid.
        let legacy = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        for dir in [&current, &legacy] {
            std::fs::create_dir_all(dir.join("bin")).expect("build the fixture");
        }

        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&root, prefix, socket_name, "test");
        }

        assert!(!current.exists(), "a dead current-shape directory must be reclaimed");
        assert!(
            !legacy.exists(),
            "a dead directory under the old prefix must be reclaimed too"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The clipboard socket is bound beside the switch socket only when the fake `tmux` is linked,
    /// 0600, and handed over once.
    #[test]
    fn the_clipboard_socket_is_bound_only_beside_the_shim_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut companion = PaneSwitchChannel::bind_without_shim().expect("bind");
        assert!(companion.take_clipboard_listener().is_none());
        let clip = companion
            .socket_path()
            .with_file_name(crate::host_clipboard::SOCKET_NAME);
        assert!(!clip.exists(), "no clipboard socket without the shim");
        companion.cleanup();

        let root = std::env::temp_dir().join(format!("nv-ps-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let shim = root.join("eitri-tmux-shim");
        std::fs::write(&shim, b"").unwrap();
        let mut channel = PaneSwitchChannel::bind(&shim).expect("bind");
        let clip = channel.socket_path().with_file_name(crate::host_clipboard::SOCKET_NAME);
        let mode = std::fs::symlink_metadata(&clip).expect("bound").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert!(channel.take_clipboard_listener().is_some());
        assert!(channel.take_clipboard_listener().is_none(), "handed over once");
        channel.cleanup();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The socket path at macOS's worst-case temporary directory is exactly 100 bytes.
    ///
    /// An inequality against `MAX_SOCKET_PATH_BYTES` would let growth from 100 to 103 bytes pass
    /// while silently consuming the remaining headroom. Pinning the exact length forces any
    /// increase to be deliberate, with a shorter directory or socket name if more margin is needed.
    /// Update the number deliberately; do not relax the assertion.
    #[test]
    fn the_switch_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        // `/var/folders/<2>/<28>/T/`: the measured macOS per-user temp-directory shape, 49 bytes.
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        // A synthetic 5-digit pid rather than this process's own: macOS's `PID_MAX` is 99999, so
        // five digits is the longest a pid can be *there*, and the length under test must not drift
        // with whatever pid happens to run the suite. (Linux allows seven, which is why the same
        // path can reach 102 bytes on a Linux host with a 49-byte TMPDIR -- still inside
        // Linux's own 107-byte limit, and refused before `bind` by `in_dir` regardless.)
        let dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{}", uuid::Uuid::new_v4().simple()));
        let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
        // The clipboard socket sits in the same directory under a name as long as the switch socket's.
        let clip = agent::socket_path::in_dir(&dir, crate::host_clipboard::SOCKET_NAME).expect("must fit");
        assert_eq!(clip.as_os_str().len(), 100, "{clip:?}");

        // The companion's channel is named by the same two constants, so it spends the same bytes:
        // rebuild its directory name at the macOS worst case from what `bind_without_shim` made.
        let channel = PaneSwitchChannel::bind_without_shim().expect("bind");
        let bound = channel.socket_path().to_path_buf();
        assert_eq!(bound.file_name().unwrap(), SOCKET_NAME);
        let name = bound
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let tail = name.strip_prefix(DIR_PREFIX).expect("directory prefix");
        let (pid, uuid) = tail.split_once('-').expect("pid-uuid");
        assert_eq!(pid, std::process::id().to_string());
        assert_eq!(uuid.len(), 32, "{name}");
        let companion_dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{uuid}"));
        let companion_path = agent::socket_path::in_dir(&companion_dir, SOCKET_NAME).expect("must fit");
        assert_eq!(companion_path.as_os_str().len(), 100, "{companion_path:?}");
        channel.cleanup();

        // And on whatever `TMPDIR` this host actually has, with its real pid.
        let here = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        let path = agent::socket_path::in_dir(&here, SOCKET_NAME).expect("must fit");
        assert!(
            path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
            "{path:?}"
        );
    }
}
