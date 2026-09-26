//! The pane-switch *protocol*: the private directory, the fake-`tmux` symlink, the socket the shim
//! writes one direction letter to, and the parser that reads it -- plus, since Task 4 of the
//! 2026-09-26 wave, a second message on the same socket: `Q <generation>`, sent by
//! [`crate::layout::kill::editor_quit_keys`] when a `:confirm qall` it asked nvim for is cancelled.
//! Toolkit-free.
//!
//! The half that is **not** here is the polling driver: `shell::pane_switch` owns the
//! `glib::timeout_add_local` timer that calls [`accept_pending_messages`] once a tick and hands
//! each message to a GTK focus grab or a kill-cancellation clear. That split is L2's own
//! requirement -- "协议搬进核心，轮询驱动
//! 留在壳里" (`docs/superpowers/specs/2026-09-16-macos-path-design.md`, L2) -- and it is what lets a
//! second host (the macOS spike, M3) reuse every line below with its own run-loop timer.
//!
//! The *why* of the mechanism as a whole -- why `shell` fakes tmux for the embedded nvim instead of
//! intercepting `Ctrl+h`/`Ctrl+l` itself, and why each direction means what it means -- stays in
//! `shell::pane_switch`'s own module doc, because it is about panes and focus, which this crate has
//! neither of.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a host should call [`accept_pending_messages`]. A keypress-driven focus switch at
/// this cadence is imperceptible, and an idle poll of a non-blocking `accept()` costs one failing
/// syscall.
///
/// It lives beside the protocol rather than beside the timer that consumes it because the
/// `#[ignore]`d real-nvim tests in [`crate::theme::feed`] poll at the same cadence to reproduce what
/// the main loop does; one number, two readers.
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bound on how long a single accepted connection may block the caller while its one short line
/// arrives. The shim writes immediately after `connect()` returns, so in practice this is never
/// reached; it exists because an accepted `UnixStream`'s blocking mode is not something a portable
/// caller may assume -- see [`accept_pending_messages`], which sets it explicitly.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

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

/// A live pane-switch channel: a private directory holding the fake-`tmux` symlink and the Unix
/// socket, plus the bound listener.
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
        if let Err(e) = std::fs::create_dir_all(&bin_dir) {
            eprintln!(
                "[pane_switch] could not create {}: {e} -- Ctrl+h/j/k/l out of nvim disabled",
                bin_dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[pane_switch] {what} -- Ctrl+h/j/k/l out of nvim disabled");
            let _ = std::fs::remove_dir_all(&dir);
        };

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

        println!(
            "[pane_switch] fake tmux at {}, socket at {}",
            fake_tmux.display(),
            socket_path.display()
        );
        Some(Self {
            dir,
            socket_path,
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
    pub fn child_env(&self) -> Vec<(String, String)> {
        let bin_dir = self.dir.join("bin");
        let path = match std::env::var("PATH") {
            Ok(existing) => format!("{}:{}", bin_dir.display(), existing),
            Err(_) => bin_dir.display().to_string(),
        };
        vec![
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
        ]
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
/// ([`crate::layout::kill::editor_quit_keys`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMessage {
    Direction(char),
    QuitCancelled(u32),
}

/// Every message already queued on the socket, in accept order.
///
/// Drains rather than taking one per call: a fast repeated keypress can leave more than one
/// connection pending between two polls.
///
/// This returns the whole batch before any callback runs, where the pre-L2-T5 code (in
/// `shell/src/main.rs`) fired each direction's focus-grab callback from inside the accept loop
/// itself, one connection at a time. The change is judged benign, not verified inert: accept
/// order is preserved end to end (this function's own order is the loop's iteration order, both
/// direct callers), and a focus grab is idempotent, so firing a burst of them slightly later than
/// before costs nothing observable.
///
/// **Each accepted stream is put back into blocking mode explicitly, and that line is the macOS
/// fix (L2 T5, 2026-09-17).** `listener` is non-blocking, because the host polls it from a main
/// loop that must not stall. Linux clears `O_NONBLOCK` on the fd `accept()` returns; **macOS
/// copies it from the listener** (rust-lang/rust#67027). On macOS, therefore, the `read_line`
/// below ran on a non-blocking socket and returned `WouldBlock` -- "Resource temporarily
/// unavailable (os error 35)" -- whenever the shim had not finished writing its one letter yet,
/// and the keypress was silently dropped. `READ_TIMEOUT` cannot help: a read timeout is only
/// consulted by a *blocking* socket. `agent` paid for exactly this bug in M1 (`fd095b6`); this is
/// the same fix at this workspace's other two listeners. **No Linux test can catch it**, because
/// on Linux the accepted stream is blocking whether or not this line is here.
///
/// A stream whose mode cannot be set is skipped rather than read, but unlike `agent`'s hook relay
/// -- which answers such a stream with a fail-closed deny, because it gates tool execution -- this
/// channel has nothing to fail closed *to*: a dropped message costs one focus switch or one stuck
/// kill flag the user can retry, and inventing one would move focus, or clear a kill, nobody asked
/// for.
pub fn accept_pending_messages(listener: &UnixListener) -> Vec<PaneMessage> {
    accept_pending_messages_within(listener, READ_TIMEOUT)
}

/// [`accept_pending_messages`] with the per-connection read timeout as a parameter.
///
/// Private, and it exists for the tests rather than for a second production caller: the macOS
/// regression test below has to let a connection sit accepted-but-silent for a measurable moment,
/// and with `READ_TIMEOUT` fixed at 50ms that test's only lever was a sleep short enough to race
/// its own bound. See `a_direction_written_after_the_reader_is_already_waiting_still_arrives`.
fn accept_pending_messages_within(listener: &UnixListener, read_timeout: Duration) -> Vec<PaneMessage> {
    let mut messages = Vec::new();
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = stream.set_nonblocking(false) {
                    eprintln!("[pane_switch] could not make an accepted connection blocking: {e} -- ignoring it");
                    continue;
                }
                let _ = stream.set_read_timeout(Some(read_timeout));
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_err() {
                    eprintln!("[pane_switch] failed to read a message from an accepted connection");
                    continue;
                }
                match parse_message(&line) {
                    Some(message) => messages.push(message),
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
    messages
}

/// The one line the shim or [`crate::layout::kill::editor_quit_keys`] writes, turned into a
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
            dir: dir.clone(),
            listener: None,
        };
        let env: std::collections::HashMap<String, String> = channel.child_env().into_iter().collect();

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

    /// The seam between this crate and the host: everything a driver needs per tick is one call,
    /// and it drains a burst rather than one letter per poll.
    #[test]
    fn every_pending_connection_is_drained_in_one_pass() {
        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");

        for letter in ["R", "D", "nonsense", "Q 3"] {
            let mut stream = UnixStream::connect(&socket_path).expect("connect");
            writeln!(stream, "{letter}").unwrap();
        }
        // A garbled message is dropped, and never stops the letters or the quit generation around it
        // being delivered, in accept order.
        assert_eq!(
            accept_pending_messages(&listener),
            vec![
                PaneMessage::Direction('R'),
                PaneMessage::Direction('D'),
                PaneMessage::QuitCancelled(3),
            ]
        );
        // Nothing pending: the non-blocking `accept()` returns `WouldBlock` and this returns empty
        // rather than parking the caller's main loop.
        assert!(accept_pending_messages(&listener).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The macOS regression test for PART B, and it is honest about being untestable here.
    ///
    /// The listener is non-blocking. Linux clears `O_NONBLOCK` on the fd `accept()` returns, so on
    /// Linux this passes with or without the explicit `set_nonblocking(false)` --  **this test
    /// cannot fail on this machine.** On macOS, where XNU copies the listener's flag, a `read_line`
    /// reached before the writer has written returns `WouldBlock` and the letter is dropped; every
    /// other test here writes before the accept, which is exactly why none of them discriminates.
    /// This one connects, gives the reader long enough to have reached `read_line` with nothing to
    /// read, and only then writes.
    ///
    /// **The two directions of that race are not symmetric, and only one of them is harmless.**
    ///  - Writer **early** (it wins, and the line is already buffered when the read runs): the test
    ///    still passes, on both platforms, but stops discriminating on macOS. Harmless.
    ///  - Writer **late**, past the read timeout: the read times out, the letter is dropped and the
    ///    assertion goes red -- **on either platform, with the fix in place**. That is a false red,
    ///    and it would land where it costs most: the Mac mini has 8 GB and is the environment's only
    ///    Xcode build machine, so a scheduling hiccup there is not hypothetical.
    ///
    /// So the read timeout is a parameter here, and generous: `SLACK` is three orders of magnitude
    /// past `SLEEP`, where production's `READ_TIMEOUT` is only five times it. Widening the bound
    /// costs this test nothing -- what it is *about* is whether a blocking read happens at all, not
    /// how long one is allowed to take.
    #[test]
    fn a_direction_written_after_the_reader_is_already_waiting_still_arrives() {
        /// Long enough that the reader has reached `read_line` with nothing to read.
        const SLEEP: Duration = Duration::from_millis(10);
        /// The per-connection bound for this test only. Nothing here is measuring latency, so it is
        /// set far past any plausible scheduling hiccup rather than near `READ_TIMEOUT`.
        const SLACK: Duration = Duration::from_secs(10);

        let dir = crate::instance_dir::instance_dir_path(&std::env::temp_dir(), DIR_PREFIX);
        std::fs::create_dir_all(&dir).expect("fixture");
        let socket_path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");

        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(SLEEP);
            writeln!(stream, "R").unwrap();
        });
        assert_eq!(
            accept_pending_messages_within(&listener, SLACK),
            vec![PaneMessage::Direction('R')]
        );
        writer.join().unwrap();

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
