//! The nvim → host half of the theme *protocol*: the per-window socket the embedded nvim writes
//! snapshots to, the Lua snippet that writes them, and the parser that reads them. Toolkit-free.
//!
//! Push rather than pull because a colorscheme change happens inside nvim; a host has no way to
//! know when to ask. It never touches the RPC pipe the keyboard path uses, and needs no fork change.
//!
//! The half that is **not** here is the polling driver: `shell::theme::feed` owns the
//! `glib::timeout_add_local` timer that calls [`accept_pending_lines`] once a tick and repaints.
//! Same split, and the same reason, as [`crate::pane_switch`] -- L2's "协议搬进核心，轮询驱动留在
//! 壳里" (`docs/superpowers/specs/2026-09-16-macos-path-design.md`).
//!
//! **Both halves of the Lua↔Rust contract now live in this crate** (L2 T5, 2026-09-17):
//! [`NVIM_THEME_LUA`] is `nvim_theme.lua` beside this file, and the two tests that pin it against
//! `tokens::GROUPS_READ` and [`crate::theme::payload::PAYLOAD_VERSION`] are below.
//! They used to sit in `shell`, one crate away from everything they assert about.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::theme::payload::{parse_payload, NvimThemePayload};

/// The directory-name prefix [`crate::instance_dir`] keys this module's directories on.
///
/// Terse on purpose (L2 T5, 2026-09-17). It was `neovibe-theme-`, which with a hyphenated uuid and
/// a `theme.sock` inside came to 117 bytes under macOS's 49-byte `TMPDIR` -- 14 over the 103-byte
/// limit [`agent::socket_path::MAX_SOCKET_PATH_BYTES`] enforces, so this socket could not be bound
/// on macOS at all. See `SWEPT_NAMES` for what becomes of the old directories.
pub(crate) const DIR_PREFIX: &str = "nv-th-";

/// The socket file inside that directory. `theme.sock` before L2 T5; see [`DIR_PREFIX`].
const SOCKET_NAME: &str = "t.sock";

/// The snippet's own file name. Not shortened with the others: it is `dofile`d and named by an
/// environment variable, never bound, so no `sockaddr_un` limit applies to it and legibility in a
/// `--cmd` line and in `:echo $NEOVIBE_THEME_LUA` is worth more than the four bytes.
const LUA_NAME: &str = "theme.lua";

/// Every `(directory prefix, socket file)` pair this module has ever used, current first. The
/// second row is what builds before L2 T5 (2026-09-17) wrote, swept alongside the current one for
/// the reason `crate::pane_switch`'s own `SWEPT_NAMES` gives: the sweep matches by prefix, so a
/// rename with no second sweep orphans every pre-rename directory permanently.
/// `the_sweep_still_reclaims_a_pre_l2_t5_directory` pins that the legacy row is really swept.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME), ("neovibe-theme-", "theme.sock")];

/// How often a host should call [`accept_pending_lines`]. A colorscheme change is not
/// latency-sensitive the way a keypress is; 100ms is imperceptible and an idle poll of a
/// non-blocking `accept()` costs one failing syscall.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on how long one accepted connection may block the caller. The snippet connects and writes
/// synchronously inside one autocommand (see `send` in `nvim_theme.lua`), so the line is normally
/// already in the socket when the connection is accepted; this only bounds a misbehaving writer.
/// It is consulted only by a *blocking* socket, which is why [`accept_pending_lines`] sets each
/// accepted stream's mode explicitly first.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// The Lua half of this protocol, compiled in and written into the feed's own directory at startup.
pub(crate) const NVIM_THEME_LUA: &str = include_str!("nvim_theme.lua");

/// The one `--cmd` handed to nvim. It checks the path before `dofile`, because `dofile(nil)` reads
/// stdin -- which under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken
/// snippet costs the theme, never the editor.
pub(crate) const LOADER_CMD: &str =
    "lua local p = vim.env.NEOVIBE_THEME_LUA; if p and p ~= '' then pcall(dofile, p) end";

pub struct ThemeFeed {
    dir: PathBuf,
    socket_path: PathBuf,
    lua_path: PathBuf,
    listener: Option<UnixListener>,
}

impl ThemeFeed {
    /// Sweeps stale directories, then builds this one, writes the snippet and binds the socket --
    /// or returns `None` having logged why. `None` is supported: nvim gets no extra env or args,
    /// and the window stays on `ThemeTokens::fallback()`.
    pub fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "theme");
        }

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: the socket accepts colours from anyone who can connect to it.
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            eprintln!(
                "[theme] could not create {}: {e} -- colours stay on the built-in fallback",
                dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[theme] {what} -- colours stay on the built-in fallback");
            let _ = std::fs::remove_dir_all(&dir);
        };

        let lua_path = dir.join(LUA_NAME);
        if let Err(e) = std::fs::write(&lua_path, NVIM_THEME_LUA) {
            fail(format!("could not write the nvim snippet: {e}"));
            return None;
        }
        // Through `agent::socket_path::in_dir`, this workspace's one place that knows the 103-byte
        // cap a `sockaddr_un` path has on macOS; a path over it is refused here, naming itself and
        // its length, rather than failing inside `bind` with std's own message.
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
            fail(format!("could not make the theme socket non-blocking: {e}"));
            return None;
        }
        println!("[theme] feed at {}", socket_path.display());
        Some(ThemeFeed {
            dir,
            socket_path,
            lua_path,
            listener: Some(listener),
        })
    }

    /// Set on the nvim child only, through `NeovideEditorPaneOptions::child_env`.
    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "NEOVIBE_THEME_SOCKET".to_string(),
                self.socket_path.display().to_string(),
            ),
            ("NEOVIBE_THEME_LUA".to_string(), self.lua_path.display().to_string()),
        ]
    }

    /// Passed through `NeovideEditorPaneOptions::extra_nvim_args`.
    pub fn nvim_args(&self) -> Vec<String> {
        vec!["--cmd".to_string(), LOADER_CMD.to_string()]
    }

    /// Hands the bound listener to the host's polling driver. Returns `None` the second time, which
    /// is how a host detects that it wired two drivers to one feed.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Removes the directory. Called explicitly from the host's close handler, for the reason
    /// `PaneSwitchChannel::cleanup` documents: GTK does not reliably drop signal-handler closures
    /// before the process exits, so `Drop` alone leaks.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// The bound socket's path. Production reads it through [`ThemeFeed::child_env`]; tests connect
    /// to it directly.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ThemeFeed {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// One line per pending connection, in accept order.
///
/// **Each accepted stream is put back into blocking mode explicitly, and that line is the macOS
/// fix (L2 T5, 2026-09-17).** `listener` is non-blocking, because the host polls it from a main
/// loop that must not stall. Linux clears `O_NONBLOCK` on the fd `accept()` returns; **macOS copies
/// it from the listener** (rust-lang/rust#67027). On macOS, therefore, the `read_line` below ran on
/// a non-blocking socket and returned `WouldBlock` -- "Resource temporarily unavailable (os error
/// 35)" -- whenever nvim had not finished writing its line yet, and the snapshot was silently
/// dropped, leaving the window on `ThemeTokens::fallback()` with nothing but one stderr line to say
/// so. `READ_TIMEOUT` cannot help: a read timeout is only consulted by a *blocking* socket, so
/// setting one on a non-blocking stream is a no-op dressed as a bound. `agent` paid for exactly this
/// bug in M1 (`fd095b6`); this is the same fix at this workspace's other two listeners. **No Linux
/// test can catch it**, because on Linux the accepted stream is blocking whether or not this line
/// is here.
pub fn accept_pending_lines(listener: &UnixListener) -> Vec<String> {
    accept_pending_lines_within(listener, READ_TIMEOUT)
}

/// [`accept_pending_lines`] with the per-connection read timeout as a parameter.
///
/// Private, and it exists for the tests rather than for a second production caller: the macOS
/// regression test below has to let a connection sit accepted-but-silent for a measurable moment,
/// and with `READ_TIMEOUT` fixed at 100ms that test's only lever was a sleep short enough to race
/// its own bound. See `a_payload_written_after_the_reader_is_already_waiting_still_arrives`.
fn accept_pending_lines_within(listener: &UnixListener, read_timeout: Duration) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = stream.set_nonblocking(false) {
                    eprintln!("[theme] could not make an accepted connection blocking: {e} -- ignoring it");
                    continue;
                }
                let _ = stream.set_read_timeout(Some(read_timeout));
                let mut line = String::new();
                match BufReader::new(stream).read_line(&mut line) {
                    Ok(_) => lines.push(line),
                    Err(e) => eprintln!("[theme] failed to read a payload: {e}"),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                eprintln!("[theme] accept failed: {e}");
                break;
            }
        }
    }
    lines
}

/// The last line that parses. A malformed line is logged and skipped; it never discards an
/// earlier good payload, and it never applies partially.
pub fn latest_payload(lines: Vec<String>) -> Option<NvimThemePayload> {
    let mut latest = None;
    for line in lines {
        match parse_payload(&line) {
            Ok(payload) => latest = Some(payload),
            Err(e) => eprintln!("[theme] ignoring a payload: {e}"),
        }
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::payload::PAYLOAD_VERSION;
    use crate::theme::tokens::GROUPS_READ;
    use crate::theme::ThemeTokens;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Instant;

    fn line(colors_name: &str) -> String {
        format!(
            r#"{{"v":1,"groups":{{}},"options":{{"background":"dark","guifont":"","colors_name":"{colors_name}"}}}}"#
        )
    }

    #[test]
    fn the_lua_snippet_requests_every_group_derivation_reads() {
        for group in GROUPS_READ {
            assert!(
                NVIM_THEME_LUA.contains(&format!("\"{group}\"")),
                "nvim_theme.lua does not request {group}"
            );
        }
    }

    /// Locks the other half of the Lua-to-Rust wire contract: `payload.rs` rejects any `v` that
    /// doesn't match `PAYLOAD_VERSION` whole (see its `rejects_another_version_whole` test), so a
    /// version bump on one side with no matching bump on the other silently pins every window on
    /// `ThemeTokens::fallback()` -- and under `--clean` that failure is invisible (deviation 2 makes
    /// the fallback look like nvim's own default colorscheme).
    #[test]
    fn the_lua_snippet_pushes_the_current_payload_version() {
        assert!(
            NVIM_THEME_LUA.contains(&format!("v = {PAYLOAD_VERSION},")),
            "nvim_theme.lua's payload version is not PAYLOAD_VERSION"
        );
    }

    #[test]
    fn the_loader_never_calls_dofile_without_a_path() {
        // dofile(nil) reads stdin, which under --embed is the RPC pipe.
        assert!(LOADER_CMD.contains("if p and p ~= ''"));
        assert!(LOADER_CMD.starts_with("lua "));
    }

    #[test]
    fn only_the_latest_valid_payload_in_a_tick_is_applied() {
        let got = latest_payload(vec![line("a"), "garbage".into(), line("b")]).unwrap();
        assert_eq!(got.options.colors_name, "b");
        let got = latest_payload(vec![line("a"), "garbage".into()]).unwrap();
        assert_eq!(
            got.options.colors_name, "a",
            "a bad line never discards an earlier good one"
        );
        assert!(latest_payload(Vec::new()).is_none());
    }

    #[test]
    fn a_feed_owns_a_private_directory_with_its_socket_and_snippet() {
        let feed = ThemeFeed::new().expect("feed");
        let env: std::collections::HashMap<_, _> = feed.child_env().into_iter().collect();
        let socket = std::path::PathBuf::from(&env["NEOVIBE_THEME_SOCKET"]);
        let lua = std::path::PathBuf::from(&env["NEOVIBE_THEME_LUA"]);
        assert!(socket.starts_with(&feed.dir) && lua.starts_with(&feed.dir));
        assert_eq!(std::fs::read_to_string(&lua).unwrap(), NVIM_THEME_LUA);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&feed.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(feed.nvim_args(), vec!["--cmd".to_string(), LOADER_CMD.to_string()]);
        feed.cleanup();
        assert!(!feed.dir.exists());
    }

    #[test]
    fn every_pending_connection_is_drained_in_one_pass() {
        let feed = ThemeFeed::new().expect("feed");
        for name in ["one", "two"] {
            let mut stream = UnixStream::connect(feed.socket_path()).expect("connect");
            stream.write_all(format!("{}\n", line(name)).as_bytes()).unwrap();
        }
        let lines = accept_pending_lines(feed.listener.as_ref().unwrap());
        assert_eq!(lines.len(), 2);
        assert_eq!(latest_payload(lines).unwrap().options.colors_name, "two");
        feed.cleanup();
    }

    /// The macOS regression test for PART B, and it is honest about being untestable here.
    ///
    /// The listener is non-blocking. Linux clears `O_NONBLOCK` on the fd `accept()` returns, so on
    /// Linux this passes with or without the explicit `set_nonblocking(false)` -- **this test cannot
    /// fail on this machine.** On macOS, where XNU copies the listener's flag, a `read_line` reached
    /// before nvim has written returns `WouldBlock` and the snapshot is dropped; every other test
    /// here writes before the accept, which is exactly why none of them discriminates. This one
    /// connects, gives the reader long enough to have reached `read_line` with nothing to read, and
    /// then writes a payload over 16 KiB -- larger than macOS's 8 KiB AF_UNIX stream buffer, so the
    /// line cannot arrive in one read either.
    ///
    /// **The two directions of that race are not symmetric, and only one of them is harmless.**
    ///  - Writer **early** (it wins, and the line is already buffered when the read runs): the test
    ///    still passes, on both platforms. Here it even keeps discriminating on macOS, because
    ///    32 KiB cannot sit in an 8 KiB buffer -- the read must come back for more either way.
    ///  - Writer **late**, past the read timeout: the read times out, the payload is dropped and the
    ///    assertion goes red -- **on either platform, with the fix in place**. That is a false red,
    ///    and it would land where it costs most: the Mac mini has 8 GB and is the environment's only
    ///    Xcode build machine, so a scheduling hiccup there is not hypothetical. Writing 32 KiB
    ///    through an 8 KiB buffer makes it likelier still, since the writer blocks until the reader
    ///    drains and the whole exchange has to finish inside one bound.
    ///
    /// So the read timeout is a parameter here, and generous: `SLACK` is three orders of magnitude
    /// past `SLEEP`, where production's `READ_TIMEOUT` is only five times it. Widening the bound
    /// costs this test nothing -- what it is *about* is whether a blocking read happens at all, not
    /// how long one is allowed to take.
    #[test]
    fn a_payload_written_after_the_reader_is_already_waiting_still_arrives() {
        /// Long enough that the reader has reached `read_line` with nothing to read.
        const SLEEP: Duration = Duration::from_millis(20);
        /// The per-connection bound for this test only. Nothing here is measuring latency, so it is
        /// set far past any plausible scheduling hiccup rather than near `READ_TIMEOUT`.
        const SLACK: Duration = Duration::from_secs(10);

        let feed = ThemeFeed::new().expect("feed");
        let big = line(&"z".repeat(32 * 1024));
        assert!(big.len() > 16 * 1024);
        let mut stream = UnixStream::connect(feed.socket_path()).expect("connect");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(SLEEP);
            stream.write_all(format!("{big}\n").as_bytes()).unwrap();
        });

        let lines = accept_pending_lines_within(feed.listener.as_ref().unwrap(), SLACK);
        let payload = latest_payload(lines).expect("the delayed payload must still be read");
        assert_eq!(payload.options.colors_name.len(), 32 * 1024);
        writer.join().unwrap();
        feed.cleanup();
    }

    /// The trap L2 T5's rename set: the sweep finds candidates **by prefix**, so a directory
    /// written under the old prefix stops being a candidate the moment the prefix changes -- it
    /// would sit under `TMPDIR` forever with nothing left in the code that names it. Sweeping
    /// [`SWEPT_NAMES`] rather than one pair is what closes that.
    #[test]
    fn the_sweep_still_reclaims_a_pre_l2_t5_directory() {
        // Own root, so this never touches a real window's directory and never races a sibling test.
        let root = std::env::temp_dir().join(format!("nv-th-sweeptest-{}", uuid::Uuid::new_v4().simple()));
        // pid 0 is rejected before any syscall by `pid_is_alive`, and `std::process::id()` never
        // returns it, so it is always a safe "definitely dead" pid to plant a fixture at.
        let current = root.join(format!("{DIR_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        // Exactly what a pre-L2-T5 build wrote: the old prefix and a hyphenated uuid.
        let legacy = root.join(format!("neovibe-theme-0-{}", uuid::Uuid::new_v4()));
        for dir in [&current, &legacy] {
            std::fs::create_dir_all(dir).expect("build the fixture");
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
    fn the_theme_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
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

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p neovibe-core theme::feed -- --ignored"]
    fn a_real_nvim_pushes_its_colorscheme() {
        let feed = ThemeFeed::new().expect("feed");
        let mut child = std::process::Command::new("nvim")
            .args(["--headless", "--clean"])
            .args(feed.nvim_args())
            .args(["-c", "colorscheme retrobox"])
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");

        let listener = feed.listener.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            seen = latest_payload(accept_pending_lines(listener)).filter(|p| p.options.colors_name == "retrobox");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();

        let payload = seen.expect("nvim never pushed a retrobox payload");
        let normal = &payload.groups["Normal"];
        assert!(normal.fg.is_some() && normal.bg.is_some(), "{normal:?}");
        assert_ne!(ThemeTokens::derive(&payload), ThemeTokens::fallback());
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p neovibe-core theme::feed -- --ignored"]
    fn a_busy_nvim_main_loop_does_not_lose_a_payload() {
        // A user's own VimEnter handler (session restore, a dashboard) runs after ours, which `--cmd`
        // registered first. A write that waits for nvim's loop to turn would arrive after this 400ms
        // handler -- past READ_TIMEOUT -- and the poll below would drop the connection.
        let feed = ThemeFeed::new().expect("feed");
        let mut child = std::process::Command::new("nvim")
            .args(["--headless", "--clean"])
            .args(feed.nvim_args())
            .args([
                "-c",
                "autocmd VimEnter * lua vim.uv.sleep(400)",
                "-c",
                "colorscheme retrobox",
            ])
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");

        let listener = feed.listener.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = Vec::new();
        // Polled at POLL_INTERVAL, exactly as `shell::theme::feed::listen` polls on the GTK main loop.
        while Instant::now() < deadline && received.len() < 2 {
            received.extend(
                accept_pending_lines(listener)
                    .iter()
                    .filter_map(|line| parse_payload(line).ok()),
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();

        assert!(
            received.len() >= 2,
            "expected the ColorScheme and the VimEnter payload, got {}",
            received.len()
        );
        assert!(received.iter().all(|p| p.options.colors_name == "retrobox"));
    }
}
