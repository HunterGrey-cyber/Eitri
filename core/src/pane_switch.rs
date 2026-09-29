//! The pane-switch *protocol*: the private directory, the fake-`tmux` symlink, the socket the shim
//! writes one direction letter to, and the parser that reads it -- plus, since Task 4 of the
//! 2026-09-26 wave, a second message on the same socket: `Q <generation>`, sent by
//! [`crate::layout::kill::editor_quit_lua`] when a `:confirm qall` it asked nvim for is cancelled.
//! Toolkit-free.
//!
//! Since v1 (spec `2026-09-27-v1-ui-design.md` §5, P1/P14) the same directory also holds
//! `nav_fallback.lua`, a `--cmd`-loaded snippet that gives `Ctrl+h/j/k/l` a way out of the editor
//! when vim-tmux-navigator is absent, and from Visual mode, writing the same letters to the same
//! socket as the shim. It lives here because leaving can only work where the socket does.
//!
//! The half that is **not** here is the polling driver: `shell::pane_switch` owns the
//! `glib::timeout_add_local` timer that constructs a [`PaneSwitchReader`] once and calls
//! [`PaneSwitchReader::poll`] every tick, handing each message to a GTK focus grab or a
//! kill-cancellation clear. That split is L2's own requirement -- "协议搬进核心，轮询驱动
//! 留在壳里" (`docs/superpowers/specs/2026-09-16-macos-path-design.md`, L2) -- and it is what lets a
//! second host (the macOS spike, M3) reuse every line below with its own run-loop timer.
//!
//! The *why* of the mechanism as a whole -- why `shell` fakes tmux for the embedded nvim instead of
//! intercepting `Ctrl+h`/`Ctrl+l` itself, and why each direction means what it means -- stays in
//! `shell::pane_switch`'s own module doc, because it is about panes and focus, which this crate has
//! neither of.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::DirBuilderExt;
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
/// connection once, so it always makes progress (fix round 2, P1 audit).
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
/// exceed a typical 1024-fd soft `RLIMIT_NOFILE` (fix round 1, P1 audit: a same-user, 0700-directory
/// sender gains nothing from this beyond running its own process out of descriptors first, but the
/// bound belongs in the type rather than only in the two numbers above). Once the cap is hit,
/// `poll` simply stops accepting for the rest of that call; a burst past it waits in the kernel's
/// own backlog the same way a burst past [`MAX_ACCEPTS_PER_POLL`] already does.
const MAX_PENDING_CONNECTIONS: usize = 64;

/// The directory-name prefix [`crate::instance_dir`] keys this module's directories on.
///
/// Terse on purpose (L2 T5, 2026-09-17). It was `neovibe-pane-switch-`, which with a hyphenated
/// uuid and a `switch.sock` inside came to 124 bytes under macOS's 49-byte `TMPDIR` -- 21 over the
/// 103-byte limit [`agent::socket_path::MAX_SOCKET_PATH_BYTES`] enforces, so this socket could not
/// be bound on macOS at all. See `SWEPT_NAMES` for what becomes of the old directories.
pub(crate) const DIR_PREFIX: &str = "nv-ps-";

/// The socket file inside that directory. `switch.sock` before L2 T5; see [`DIR_PREFIX`].
const SOCKET_NAME: &str = "s.sock";

/// Every `(directory prefix, socket file)` pair this module has ever used, current first.
///
/// [`sweep_stale_dirs`] walks all of them, so a directory a pre-rename build leaked is still
/// reclaimed rather than left under `TMPDIR` forever: the sweep matches **by prefix**, so a rename
/// with no second sweep would orphan every such directory permanently, with nothing left in the
/// code that names it. A pre-rename build that is still *running* is spared by the same two checks
/// as always -- its pid is alive, and its `switch.sock` answers a `connect()`.
/// `the_sweep_still_reclaims_a_pre_l2_t5_directory` pins that the legacy row is really swept and
/// not merely written down.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME), ("neovibe-pane-switch-", "switch.sock")];

/// The nav fallback's file name inside the channel's directory. `dofile`d through an environment
/// variable, never bound, so no `sockaddr_un` limit applies to it.
const NAV_LUA_NAME: &str = "nav_fallback.lua";

/// The nav fallback itself (spec `2026-09-27-v1-ui-design.md` §5): a Lua copy of
/// vim-tmux-navigator's `s:TmuxAwareNavigate` for Normal mode, plus a Visual-mode one that keeps the
/// selection at nvim's edge, installed only where the global slot is empty, nvim's own default or a
/// plain `<C-W>{dir}` move.
pub(crate) const NAV_FALLBACK_LUA: &str = include_str!("nav_fallback.lua");

/// The one `--cmd` (spec §5.1), guarded the way `nvim_keys`'s loader is: the path is checked before
/// `dofile`, because `dofile(nil)` reads stdin -- under `--embed`, the RPC pipe -- and the load is
/// `pcall`ed, so a broken snippet costs the fallback, never the editor.
pub(crate) const NAV_LOADER_CMD: &str =
    "lua local p = vim.env.NEOVIBE_NAV_LUA; if p and p ~= '' then pcall(dofile, p) end";

/// A live pane-switch channel: a private directory holding the fake-`tmux` symlink, the Unix
/// socket and the nav fallback's snippet, plus the bound listener.
///
/// Both `Drop` and the explicit [`PaneSwitchChannel::cleanup`] remove that whole directory, but
/// **neither is guaranteed to run**, so this type does not promise that nothing is left behind. In
/// `shell` the value lives inside the window's `close-request` closure, which GTK was observed not
/// to free before the process exits (see [`PaneSwitchChannel::cleanup`]'s own doc for that finding,
/// and why the close handler calls it explicitly), and no destructor runs at all on SIGKILL or a
/// hard crash. What survives such an exit is a dead symlink and a dead socket under `TMPDIR`;
/// [`sweep_stale_dirs`] is what reclaims them on a later launch.
pub struct PaneSwitchChannel {
    dir: PathBuf,
    socket_path: PathBuf,
    /// `None` when the snippet could not be written: the channel still serves the shim, and nvim
    /// simply gets no fallback (neither `NEOVIBE_NAV_LUA` nor the loader).
    nav_lua: Option<PathBuf>,
    listener: Option<UnixListener>,
}

/// Reclaims directories this module's earlier, now-dead processes left behind, under both the
/// current and the pre-L2-T5 names.
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
        // or rewrite the Lua -- code run as this user (local-IPC review finding 2, ruling R5).
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
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
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&bin_dir) {
            fail(format!("could not create {}: {e}", bin_dir.display()));
            return None;
        }

        let fake_tmux = bin_dir.join("tmux");
        if let Err(e) = std::os::unix::fs::symlink(shim, &fake_tmux) {
            fail(format!(
                "could not link {} -> {}: {e}",
                fake_tmux.display(),
                shim.display()
            ));
            return None;
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
        if let Err(e) = listener.set_nonblocking(true) {
            fail(format!("could not set the switch socket non-blocking: {e}"));
            return None;
        }

        // Optional, unlike everything above: without it vim-tmux-navigator still leaves through the
        // shim, so a failed write costs only the fallback (spec §5), never the channel.
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

        println!(
            "[pane_switch] fake tmux at {}, socket at {}",
            fake_tmux.display(),
            socket_path.display()
        );
        Some(Self {
            dir,
            socket_path,
            nav_lua,
            listener: Some(listener),
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
    /// `NEOVIBE_NAV_LUA` names the nav fallback's snippet, which [`Self::nvim_args`]'s loader runs;
    /// it is absent when the snippet could not be written, and the loader is then a no-op.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let bin_dir = self.dir.join("bin");
        let path = match std::env::var("PATH") {
            Ok(existing) => format!("{}:{}", bin_dir.display(), existing),
            Err(_) => bin_dir.display().to_string(),
        };
        let mut env = vec![
            (
                "TMUX".to_string(),
                format!("{},{},0", self.socket_path.display(), std::process::id()),
            ),
            ("TMUX_PANE".to_string(), "%0".to_string()),
            ("PATH".to_string(), path),
            (
                "NEOVIBE_PANE_SWITCH_SOCKET".to_string(),
                self.socket_path.display().to_string(),
            ),
        ];
        if let Some(nav_lua) = &self.nav_lua {
            env.push(("NEOVIBE_NAV_LUA".to_string(), nav_lua.display().to_string()));
        }
        env
    }

    /// The nav fallback's `--cmd` loader (spec §5.1), for the nvim child only, beside the theme,
    /// editor-context and nvim-keys feeds' own. Empty when the snippet could not be written.
    pub fn nvim_args(&self) -> Vec<String> {
        if self.nav_lua.is_none() {
            return Vec::new();
        }
        vec!["--cmd".to_string(), NAV_LOADER_CMD.to_string()]
    }

    /// Hands the bound listener to the host's polling driver. Returns `None` the second time, which
    /// is how a host detects that it wired two drivers to one channel.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Removes the shim directory (fake `tmux` symlink and socket alike). Idempotent, and safe to
    /// call while the nvim child is still alive -- by the time a host calls this it is shutting
    /// that child down anyway, and a missing `tmux` only means the plugin's `system()` call finds
    /// nothing to run.
    ///
    /// **This exists because `Drop` alone was observed not to be enough.** In the sandbox, closing
    /// the real `shell` window left the shim directory behind every time: the channel is owned by
    /// the window's `close-request` closure, and GTK does not deterministically free
    /// signal-handler closures before the process exits, so its `Drop` never ran. That matters
    /// more now than it did: a uuid-keyed directory is never reused, so a skipped cleanup leaks
    /// rather than being overwritten by the next run at the same pid. [`sweep_stale_dirs`]
    /// reclaims such a directory at the *next* launch, which is a backstop for the paths no
    /// destructor can reach (SIGKILL, a hard crash) -- not a reason to skip this call, which is
    /// what keeps a normal close from leaving anything behind at all. `Drop` is kept below for
    /// every other path (an early failure, a future caller that owns this on the stack).
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// The bound socket's path. Only the `TMUX`/`NEOVIBE_PANE_SWITCH_SOCKET` env pairs need it in
    /// production; tests read it to connect.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for PaneSwitchChannel {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// A message accepted off the socket: a pane-switch direction letter from the `vim-tmux-navigator`
/// shim, or a kill's generation from a cancelled `:confirm qall`
/// ([`crate::layout::kill::editor_quit_lua`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMessage {
    Direction(char),
    QuitCancelled(u32),
}

/// Reads [`PaneMessage`]s off a pane-switch socket without ever blocking the caller on a slow or
/// trickling sender (P3-A1, the pane-switch half; the theme feed's identical mechanism was already
/// fixed as `sw-theme-1`). Construct once per channel and retain across polls -- a fresh reader
/// per call loses every partial connection's buffered bytes and reintroduces the bug below.
///
/// **Before this (P3-A1):** each accepted connection was read with a blocking `read_line` under a
/// per-connection `SO_RCVTIMEO`. That timeout is only ever consulted between two reads that each
/// return *something*; a sender trickling its line in slowly (one byte at a time, well under the
/// timeout) resets the clock on every successful partial read and holds the caller -- and the GTK
/// main loop it runs on -- for the whole trickle rather than for the stated timeout. Measured at
/// **3.2x the stated bound** for this module specifically -- 160.8ms against a 50ms `READ_TIMEOUT`
/// (`the private review notes`, "P3-A1"; that verdict's "18x" figure is
/// the *theme feed's* own measurement on its 100ms timeout, a different module fixed separately as
/// `sw-theme-1` -- fix round 1, P1 audit, corrected a copy-paste of that number into this module's
/// own doc) -- and reproduced again at this base, against this module's own old `accept_pending_messages`, before
/// this fix landed -- see `p3_a1_a_trickled_message_never_holds_one_poll_past_its_budget`'s own doc.
/// `poll` never blocks on a read: every read is non-blocking, and an incomplete connection is kept
/// (not waited on) for the next call.
///
/// **Delivers every message in accept order, and never lets a newer connection's message overtake
/// an older, still-incomplete one.** [`crate::line_feed::NewestLineReader`] is not reused here for
/// exactly that reason -- it is *newest-wins*, built for a feed where only the latest snapshot
/// matters and an older one may be silently superseded. A pane-switch direction is a discrete
/// keypress: dropping or reordering one would move focus, or clear a kill flag, somewhere the user
/// never asked for (ruling R5, `docs/superpowers/plans/2026-09-28-v1-audit-fixes.md`). `poll` only
/// ever reads its oldest pending connection; a newer one is not looked at until the older
/// completes, is closed, or ages out (see [`MAX_PENDING_AGE`]) -- so a stuck sender blocks the
/// messages behind it rather than letting them overtake it.
///
/// **Each accepted stream is made non-blocking explicitly.** `listener` is already non-blocking
/// (the host polls it from a main loop that must not stall), and Linux does *not* carry that flag
/// onto the fd `accept()` returns -- an accepted stream defaults to blocking there unless this is
/// done, which would silently reintroduce the same hang this type exists to remove. (macOS copies
/// the listener's flag instead, `rust-lang/rust#67027`, so it needed the opposite explicit call
/// under the old blocking-read design -- see the dated record's L2 T5 entry for that half of the
/// history.) A stream whose mode cannot be set is skipped, and one that grows past
/// [`MAX_MESSAGE_BYTES`] is dropped with a log rather than retained without bound -- like the old
/// code, this channel has nothing to fail closed *to*: a dropped message costs one focus switch or
/// one stuck kill flag the user can retry, not a security boundary.
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

    /// Every message that became complete since the last call, in accept order. Never blocks: a
    /// connection with no complete line yet is retained for the next call rather than waited on --
    /// see this type's own doc for why that is the fix, and why a stuck connection blocks the
    /// messages behind it rather than letting a later one overtake it.
    pub fn poll(&mut self) -> Vec<PaneMessage> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Vec<PaneMessage> {
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            // Fix round 1 (P1 audit, [`MAX_PENDING_CONNECTIONS`]'s own doc): stop accepting once the
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

        // Fix round 1 (P1 audit): staleness eviction is merged into the read loop below rather than
        // run as its own pass beforehand. The earlier two-pass shape evicted every stale connection
        // at the *front* before ever attempting a read on any of them -- so a silent connection and
        // a connection whose complete message was already sitting unread, accepted in the same poll
        // (sharing one `accepted_at`), both aged out together: the sweep dropped the silent one,
        // found the next front equally stale, and dropped that one too without ever calling
        // `read_available` on it, discarding an already-complete message. Reading it here, in the
        // same pass that decides staleness, means a front that is old enough to evict is still tried
        // once first -- if that read turns out `Complete`, the message is delivered instead of lost;
        // only a front that is *still* `Pending` after that read is the "never completed a message"
        // case the eviction log line describes.
        //
        // Fix round 2 (P1 audit): the time budget starts *here*, on the real clock, and is checked
        // after each read rather than before the first one. It used to be `now + MAX_POLL_DURATION`
        // checked at the top of this loop, which (a) charged the accept loop above -- already bounded
        // on its own by `MAX_ACCEPTS_PER_POLL` -- against the read budget, and (b) let a poll that had
        // been preempted for 5ms before reaching this line return nothing at all, however complete its
        // front message was. `now` is still the clock for *ageing* (tests move it forward to age a
        // connection), but it is the wrong clock for a wall-clock budget: a `now` in the future
        // disabled the budget and one in the past exhausted it before any work. Every poll now
        // reads its front connection at least once, so each makes progress; the loop stays bounded,
        // since every pass either removes the front connection or stops.
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
                        None => eprintln!("[pane_switch] ignoring unrecognized message {text:?}"),
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

/// The one line the shim or [`crate::layout::kill::editor_quit_lua`] writes, turned into a
/// [`PaneMessage`]. Anything else is rejected rather than guessed at -- the socket is only ever
/// written to by those two senders, but a garbled message must not be able to move the user's focus
/// or clear a kill nobody asked for.
///
/// The quit-generation form is parsed strictly: `"Q "` followed by one or more ASCII digits and
/// nothing else, checked digit-by-digit before `parse` -- `u32::from_str` alone accepts a leading
/// `"+"` (`"Q +7"` would otherwise parse), and a byte check is what rejects a second run of digits
/// (`"Q 7 8"`), a second space (`"Q  7"`) or an out-of-range value (`"Q 4294967296"`) instead of
/// truncating or panicking.
fn parse_message(line: &str) -> Option<PaneMessage> {
    let trimmed = line.trim();
    match trimmed {
        "L" => return Some(PaneMessage::Direction('L')),
        "R" => return Some(PaneMessage::Direction('R')),
        "U" => return Some(PaneMessage::Direction('U')),
        "D" => return Some(PaneMessage::Direction('D')),
        _ => {}
    }
    let rest = trimmed.strip_prefix("Q ")?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        rest.parse::<u32>().ok().map(PaneMessage::QuitCancelled)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    /// For a test asserting that **one** poll handles several connections (fix round 2, P1 audit).
    /// [`MAX_POLL_DURATION`] is a wall-clock backstop, so under it such an assertion depends on the
    /// scheduler: a stall of 5ms between two reads legitimately leaves the rest for the next poll.
    /// The budget's own behaviour is pinned separately, at zero, by
    /// `a_poll_past_its_time_budget_still_delivers_its_front_message_and_keeps_the_rest_in_order`.
    const UNHURRIED: Duration = Duration::from_secs(60);

    /// Test-only safety net (fix round 1, P1 audit): if a regression ever drops the explicit
    /// `set_nonblocking(true)` [`PaneSwitchReader::poll`] depends on, the "must never block" family
    /// of tests below would call a `poll()` that can block forever on a real syscall -- turning a
    /// clean, fast test failure into a hung test *process* (mutation-checked: setting an accepted
    /// stream back to blocking makes several of these tests run past 60s rather than fail). Running
    /// the call on its own thread and giving up after `timeout` keeps the failure immediate: on the
    /// fast path the reader (and its result) come back through the channel and the caller keeps
    /// using it exactly as before; on a timeout the spawned thread -- still parked in the blocked
    /// syscall, holding the reader -- is abandoned rather than joined. That leaked thread is
    /// harmless here: it dies with the test binary's own process exit, and it touches nothing beyond
    /// this one test's own socket and directory.
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

    #[test]
    fn each_shim_directory_is_unique_but_still_names_its_process() {
        let tmp = std::env::temp_dir();
        let a = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        let b = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // The whole point of A5: two channels built in one process get two directories, so neither
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
            dir: dir.clone(),
            listener: None,
        };
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();
        assert_eq!(env["NEOVIBE_NAV_LUA"], dir.join(NAV_LUA_NAME).display().to_string());
        assert!(std::env::var_os("NEOVIBE_NAV_LUA").is_none());

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
            env["NEOVIBE_PANE_SWITCH_SOCKET"],
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
        assert!(std::env::var_os("NEOVIBE_PANE_SWITCH_SOCKET").is_none());

        // `channel` was built by hand and owns no real directory; make sure Drop's remove_dir_all
        // can't take out anything real if this test's temp path ever happened to exist.
        std::mem::forget(channel);
    }

    /// A channel whose snippet could not be written still serves the shim, and hands nvim neither
    /// half of the fallback -- a loader with no `NEOVIBE_NAV_LUA` would be a no-op anyway, but an
    /// argument that does nothing is one more thing to misread in `ps`.
    #[test]
    fn without_its_snippet_the_channel_hands_nvim_no_fallback() {
        let dir = std::env::temp_dir().join("nv-ps-unit-test-no-nav");
        let channel = PaneSwitchChannel {
            socket_path: dir.join(SOCKET_NAME),
            nav_lua: None,
            dir: dir.clone(),
            listener: None,
        };
        assert!(channel.child_env().iter().all(|(k, _)| k != "NEOVIBE_NAV_LUA"));
        assert!(channel.nvim_args().is_empty());
        std::mem::forget(channel);
    }

    /// `bind` writes the snippet into the channel's own directory, names it to the child, and
    /// hands nvim the loader -- and `cleanup` takes it away with the rest of the directory.
    #[test]
    fn bind_writes_the_nav_fallback_beside_the_socket() {
        // The shim is only a symlink's target here; nothing runs it.
        let channel = PaneSwitchChannel::bind(Path::new("/nonexistent/neovibe-tmux-shim")).expect("bind");
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();
        let nav = PathBuf::from(&env["NEOVIBE_NAV_LUA"]);
        assert_eq!(nav.parent(), channel.socket_path().parent());
        assert_eq!(nav.file_name().unwrap(), NAV_LUA_NAME);
        assert_eq!(std::fs::read_to_string(&nav).unwrap(), NAV_FALLBACK_LUA);
        assert_eq!(
            channel.nvim_args(),
            vec!["--cmd".to_string(), NAV_LOADER_CMD.to_string()]
        );
        channel.cleanup();
        assert!(!nav.exists());
    }

    /// Local-IPC review finding 2 (ruling R5): the instance directory and `bin/` -- first on nvim's
    /// `PATH` -- are 0700 and the `dofile`d snippet 0600, not the umask's defaults (`0775`/`0664`
    /// under umask 002 before this, `0755`/`0644` under 022).
    #[test]
    fn the_shim_directory_and_the_nav_snippet_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let channel = PaneSwitchChannel::bind(Path::new("/nonexistent/neovibe-tmux-shim")).expect("bind");
        let dir = channel.socket_path().parent().unwrap().to_path_buf();
        assert_eq!(mode(&dir), 0o700, "the instance directory");
        assert_eq!(mode(&dir.join("bin")), 0o700, "bin/, first on nvim's PATH");
        assert_eq!(mode(&dir.join(NAV_LUA_NAME)), 0o600, "the snippet nvim runs");
        channel.cleanup();
    }

    #[test]
    fn the_nav_loader_checks_its_path_before_dofile() {
        assert!(NAV_LOADER_CMD.starts_with("lua "));
        assert!(NAV_LOADER_CMD.contains("vim.env.NEOVIBE_NAV_LUA"));
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
            "vim.env.NEOVIBE_PANE_SWITCH_SOCKET",
            "letter .. \"\\n\"",
            "neovibe: window or pane ",
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

    /// Fix round 2 (P1 audit): the time budget is a backstop, never a reason for a poll to make no
    /// progress. Before this, `poll_at` checked its deadline -- the poll's *start* plus
    /// [`MAX_POLL_DURATION`] -- before its first read, so a poll whose accepts (or a preemption) had
    /// already used up those 5ms returned nothing at all, even with a complete message at the front.
    /// That is what made this module's own suite fail `left: []` in about one run in ten under
    /// parallel load. A zero budget is that case made deterministic: every poll here is past its
    /// deadline before it reads anything, and must still deliver its front connection's message --
    /// one per poll, the rest kept for later polls in accept order.
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

    /// **P3-A1 (pane-switch half).** Before this fix, `accept_pending_messages` read with a
    /// blocking `read_line` under a per-connection `SO_RCVTIMEO`; that timeout is reset by every
    /// successful partial read, so a sender trickling its line in slowly held the caller -- and the
    /// GTK main loop it ran on -- for the whole trickle rather than for the stated timeout, measured
    /// at 3.2x this module's own stated bound (`the private review notes`,
    /// "P3-A1" -- its "18x" figure is the theme feed's own, separate measurement; fix round 1, P1
    /// audit).
    /// Reproduced against this crate's own code at this task's base commit before this fix landed
    /// (81ms for an ~80ms trickle, against a 30ms budget -- the red half of this task's TDD cycle).
    ///
    /// `PaneSwitchReader::poll` never blocks on a read: every read is non-blocking, and a connection
    /// with no complete line yet is retained rather than waited on. So every individual poll call
    /// here should return almost immediately, however slowly the sender writes, and the message must
    /// still arrive -- complete -- once enough polls have run.
    #[test]
    fn p3_a1_a_trickled_message_never_holds_one_poll_past_its_budget() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let mut reader = PaneSwitchReader::new(listener);

        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        let writer = std::thread::spawn(move || {
            // Leading spaces are trimmed by `parse_message`, so this still parses as `Direction('R')`
            // -- same shape as the verdict's own probe, enough bytes to make a stalled read measurable.
            for byte in b"      R\n" {
                std::thread::sleep(Duration::from_millis(10));
                stream.write_all(&[*byte]).unwrap();
            }
        });

        let mut messages = Vec::new();
        let mut worst = Duration::ZERO;
        for _ in 0..20 {
            let before = Instant::now();
            messages.extend(reader.poll());
            worst = worst.max(before.elapsed());
            if !messages.is_empty() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        writer.join().unwrap();

        assert_eq!(messages, vec![PaneMessage::Direction('R')]);
        assert!(
            worst < Duration::from_millis(20),
            "one poll took {worst:?}, expected well under one POLL_INTERVAL ({POLL_INTERVAL:?})"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The portability half of the fix, and now meaningful on this machine rather than only on
    /// macOS. **Each accepted stream must be made non-blocking explicitly**, or a read on it can
    /// block outright rather than merely miss a `WouldBlock`. Before this fix that risk was
    /// macOS-only (it copies the listener's non-blocking flag onto an accepted stream,
    /// `rust-lang/rust#67027`) because production wanted a *blocking* read there; this fix wants a
    /// *non-blocking* one, so the platform this matters on has flipped -- **Linux does not carry a
    /// listener's non-blocking flag onto the socket `accept()` returns**, so a missing
    /// `set_nonblocking(true)` in [`PaneSwitchReader::poll`] would default the accepted stream to
    /// blocking right here. This is what would catch it: a connection accepted but with nothing
    /// written yet must not make `poll` block.
    #[test]
    fn a_connection_with_nothing_written_yet_never_blocks_a_poll() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let reader = PaneSwitchReader::new(listener);

        let mut stream = UnixStream::connect(&socket_path).expect("connect");

        let before = Instant::now();
        let (mut reader, empty) = poll_within(reader, Duration::from_secs(2), None);
        assert!(empty.is_empty(), "nothing written yet");
        let elapsed = before.elapsed();
        // A blocking accepted stream would hold this poll until poll_within's 2 s cap, so the bound only has to
        // tell "returned at once" from "blocked". 20 ms was tripped by scheduler delay alone on a loaded host
        // (28.75 ms at a load average of ~20 on 18 cores, 2026-09-29); 500 ms keeps the whole margin to 2 s.
        assert!(
            elapsed < Duration::from_millis(500),
            "poll blocked for {elapsed:?} on an accepted, silent connection"
        );

        stream.write_all(b"R\n").unwrap();
        assert_eq!(reader.poll(), vec![PaneMessage::Direction('R')]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5's core guarantee, adversarially: an older, still-incomplete connection must not let a
    /// complete *newer* one overtake it. If it did, two keys pressed back to back could reach the
    /// host out of order -- moving focus somewhere the user never asked for.
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

    /// Fix round 1 (P1 audit, both reviewers): a stale, silent connection and a *complete* one
    /// accepted in the same poll (so they share one `accepted_at`) must not both be dropped by the
    /// age sweep. Before this fix, aging out the front connection re-checked the new front against
    /// the same cutoff without ever reading it -- so a connection whose message was already sitting
    /// unread, simply because it happened to queue up behind a connection that never wrote anything,
    /// was discarded along with the one that actually earned the eviction. Reproduced against this
    /// module's pre-fix `poll_at` before landing this test (the ready sibling's `D` was lost, the
    /// log line named "never completed a message" even though it had).
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

        // Both accepted in the same poll, so both get the same `accepted_at` -- the shape the
        // finding needs: a silent connection ahead of one whose whole message is already written.
        let _silent = UnixStream::connect(&socket_path).expect("connect (silent)");
        let mut ready = UnixStream::connect(&socket_path).expect("connect (ready)");
        ready.write_all(b"D\n").unwrap();

        let now = Instant::now();
        // First poll: the silent front is `Pending` and not yet stale, so the loop stops there --
        // the ready sibling is never looked at yet, by design (never skip ahead of a young front).
        // Both polls read a connection that never writes, so both go through `poll_within` (fix
        // round 2): under a blocking-read regression they would otherwise hang the test binary.
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

    /// Fix round 1 (P1 audit, both reviewers): [`MAX_PENDING_CONNECTIONS`] stops the pending queue
    /// from growing without bound behind a stuck front -- a same-user sender that connects without
    /// writing must not be able to accumulate an unbounded number of open descriptors before its own
    /// age evicts even the first one.
    ///
    /// Fix round 2 (P1 audit): one batch past the cap is enough to prove it (`MAX_PENDING_CONNECTIONS
    /// + MAX_ACCEPTS_PER_POLL` clients, not four times the cap), and the clients connect one batch
    /// per poll. That keeps the kernel's own listen backlog at one batch at most: a blocking
    /// `AF_UNIX` `connect()` on Linux *waits* for backlog space rather than failing, with no timeout
    /// on a std socket, so a flood connected all at once would hang this test on a host whose
    /// `net.core.somaxconn` is below it (std listens with a backlog of -1, i.e. `somaxconn`). The
    /// earlier version also only asserted `<=` the cap, which passed with nothing accepted at all.
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

    /// The trap L2 T5's rename set: [`sweep_stale_instance_dirs`] finds candidates **by prefix**,
    /// so a directory written under the old prefix stops being a candidate the moment the prefix
    /// changes -- it would sit under `TMPDIR` forever, holding a dead socket and a dead symlink,
    /// with nothing left in the code that names it. Sweeping [`SWEPT_NAMES`] rather than one pair
    /// is what closes that, and this asserts it against a real fixture of both shapes.
    ///
    /// [`sweep_stale_instance_dirs`]: crate::instance_dir::sweep_stale_instance_dirs
    #[test]
    fn the_sweep_still_reclaims_a_pre_l2_t5_directory() {
        // Own root, so this never touches a real window's directory and never races the sibling
        // tests that plant fixtures of their own.
        let root = std::env::temp_dir().join(format!("nv-ps-sweeptest-{}", uuid::Uuid::new_v4().simple()));
        // pid 0 is rejected before any syscall by `pid_is_alive`, and `std::process::id()` never
        // returns it, so it is always a safe "definitely dead" pid to plant a fixture at.
        let current = root.join(format!("{DIR_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        // Exactly what a pre-L2-T5 build wrote: the old prefix and a hyphenated uuid.
        let legacy = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        for dir in [&current, &legacy] {
            std::fs::create_dir_all(dir.join("bin")).expect("build the fixture");
        }

        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&root, prefix, socket_name, "test");
        }

        assert!(!current.exists(), "a dead current-shape directory must be reclaimed");
        assert!(!legacy.exists(), "a dead pre-L2-T5 directory must be reclaimed too");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PART C's measurement, as a test rather than a comment: the socket path at macOS's own worst
    /// case, asserted **exactly** rather than against the limit.
    ///
    /// `assert_eq!(…, 100)` and not `<= MAX_SOCKET_PATH_BYTES`, because an inequality lets the
    /// whole margin be eaten in silence -- a later change taking this from 100 to 103 would pass a
    /// `<=` check and leave zero headroom with nothing anywhere saying so. Spending one of these
    /// bytes is a real decision (see `docs/canonical/dated_record.md`'s L2 T5 follow-up entry for the
    /// ranked ways to buy more), so it should cost a red test on Linux and a sentence in a commit
    /// message, not nothing. Update the number deliberately; do not relax the assertion.
    #[test]
    fn the_switch_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        // `/var/folders/<2>/<28>/T/` -- this project's Mac mini, measured 2026-09-17. Every macOS
        // user's per-user temp dir has that shape, so this is the real worst case there.
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

        // And on whatever `TMPDIR` this host actually has, with its real pid.
        let here = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        let path = agent::socket_path::in_dir(&here, SOCKET_NAME).expect("must fit");
        assert!(
            path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
            "{path:?}"
        );
    }
}
