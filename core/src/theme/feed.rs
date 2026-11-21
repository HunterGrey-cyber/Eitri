//! The nvim → host half of the theme *protocol*: the per-window socket the embedded nvim writes
//! snapshots to, the Lua snippet that writes them, and the parser that reads them. Toolkit-free.
//!
//! Push rather than pull because a colorscheme change happens inside nvim; a host has no way to
//! know when to ask. It never touches the RPC pipe the keyboard path uses, and needs no fork change.
//!
//! The half that is **not** here is the polling driver: `shell::theme::feed` owns the
//! `glib::timeout_add_local` timer that calls [`ThemePayloadReader::poll`] once a tick and repaints.
//! Same split, and the same reason, as [`crate::pane_switch`]: the protocol in the core and the
//! polling driver in the shell, so another host reuses the protocol with its own timer.
//!
//! **Both halves of the Lua↔Rust contract now live in this crate** (2026-09-17):
//! [`NVIM_THEME_LUA`] is `nvim_theme.lua` beside this file, and the two tests that pin it against
//! `tokens::GROUPS_READ` and [`crate::theme::payload::PAYLOAD_VERSION`] are below.
//! They used to sit in `shell`, one crate away from everything they assert about.
//!
//! **Since 2026-09-27:** the accept path used to put every accepted stream back into
//! BLOCKING mode and `read_line` it with only a per-syscall `SO_RCVTIMEO`, so a sender trickling
//! bytes slower than that timeout -- or simply several silent connections in a row -- blocked the
//! GTK thread that polls this feed for as long as it kept going, with no cap on a line's size at
//! all. It now shares [`crate::line_feed::NewestLineReader`] with `editor_context` and `nvim_keys`:
//! non-blocking, a bounded per-poll read budget, a bounded line size and a bounded connection
//! lifetime. See that module's doc for the bounds themselves.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::line_feed::NewestLineReader;
use crate::theme::payload::{parse_payload, NvimThemePayload};

/// The directory-name prefix [`crate::instance_dir`] keys this module's directories on.
///
/// Terse on purpose (2026-09-17). It was `neovibe-theme-`, which with a hyphenated uuid and
/// a `theme.sock` inside came to 117 bytes under macOS's 49-byte `TMPDIR` -- 14 over the 103-byte
/// limit [`agent::socket_path::MAX_SOCKET_PATH_BYTES`] enforces, so this socket could not be bound
/// on macOS at all. See `SWEPT_NAMES` for what becomes of the old directories.
pub(crate) const DIR_PREFIX: &str = "nv-th-";

/// The socket file inside that directory. `theme.sock` before 2026-09-17; see [`DIR_PREFIX`].
const SOCKET_NAME: &str = "t.sock";

/// The snippet's own file name. Not shortened with the others: it is `dofile`d and named by an
/// environment variable, never bound, so no `sockaddr_un` limit applies to it and legibility in a
/// `--cmd` line and in `:echo $EITRI_THEME_LUA` is worth more than the four bytes.
const LUA_NAME: &str = "theme.lua";

/// Every `(directory prefix, socket file)` pair this module has ever used, current first. The
/// second row is what builds before 2026-09-17 wrote, swept alongside the current one for
/// the reason `crate::pane_switch`'s own `SWEPT_NAMES` gives: the sweep matches by prefix, so a
/// rename with no second sweep orphans every pre-rename directory permanently.
/// `the_sweep_still_reclaims_a_directory_under_the_old_prefix` pins that the legacy row is really swept.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME), ("neovibe-theme-", "theme.sock")];

/// How often a host should call [`ThemePayloadReader::poll`]. A colorscheme change is not
/// latency-sensitive the way a keypress is; 100ms is imperceptible and an idle poll of a
/// non-blocking `accept()` costs one failing syscall.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The Lua half of this protocol, compiled in and written into the feed's own directory at startup.
pub(crate) const NVIM_THEME_LUA: &str = include_str!("nvim_theme.lua");

/// The one `--cmd` handed to nvim. It checks the path before `dofile`, because `dofile(nil)` reads
/// stdin -- which under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken
/// snippet costs the theme, never the editor.
pub(crate) const LOADER_CMD: &str = "lua local p = vim.env.EITRI_THEME_LUA; if p and p ~= '' then pcall(dofile, p) end";

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
        let base = match crate::instance_dir::feed_base("theme snippets") {
            Ok(base) => base,
            Err(e) => {
                eprintln!("[theme] {e} -- colours stay on the built-in fallback");
                return None;
            }
        };
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_dirs_in_bases(&base, &tmp, prefix, socket_name, "theme");
        }

        let dir = crate::instance_dir::instance_dir_path(&base, DIR_PREFIX);
        // 0700: the socket accepts colours from anyone who can connect to it.
        if let Err(e) = crate::instance_dir::create_private_dir(&dir) {
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
        if let Err(e) = crate::instance_dir::make_socket_private(&socket_path) {
            fail(format!(
                "could not make {} reachable by nvim: {e}",
                socket_path.display()
            ));
            return None;
        }
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
            ("EITRI_THEME_SOCKET".to_string(), self.socket_path.display().to_string()),
            ("EITRI_THEME_LUA".to_string(), self.lua_path.display().to_string()),
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

/// The theme-feed reader: [`NewestLineReader`]'s bounded, newest-wins polling with
/// [`parse_payload`] as its acceptance test. A malformed newer payload never discards
/// the last valid one; the bounds and freshness rules are documented, and tested, in
/// [`crate::line_feed`]. **The macOS accept-mode fix (2026-09-17: an accepted stream copies
/// its listener's non-blocking flag there, unlike Linux, so it must be set explicitly) now lives
/// once in `NewestLineReader` itself rather than once per protocol.**
pub struct ThemePayloadReader(NewestLineReader);

impl ThemePayloadReader {
    /// The bounded reader underneath, for a host deciding whether there is anything to read.
    pub(crate) fn lines(&self) -> &NewestLineReader {
        &self.0
    }

    pub fn new(listener: UnixListener) -> Self {
        Self(NewestLineReader::new(listener, "theme"))
    }

    /// Returns the newest valid payload available without waiting, or `None` for no news. The
    /// caller must read `None` as "no news", never as "no theme": the host keeps whatever
    /// `ThemeTokens` it last derived.
    pub fn poll(&mut self) -> Option<NvimThemePayload> {
        self.0.poll_parsed(|line| match parse_bytes(line) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                eprintln!("[theme] ignoring a payload: {e}");
                None
            }
        })
    }
}

fn parse_bytes(line: &[u8]) -> Result<NvimThemePayload, String> {
    std::str::from_utf8(line)
        .map_err(|e| e.to_string())
        .and_then(parse_payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::payload::{CursorInfo, PAYLOAD_VERSION};
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

    fn reader_for(feed: &mut ThemeFeed) -> ThemePayloadReader {
        ThemePayloadReader::new(feed.take_listener().unwrap())
    }

    #[test]
    fn a_malformed_payload_never_discards_the_last_valid_one() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let mut good = UnixStream::connect(feed.socket_path()).unwrap();
        good.write_all(format!("{}\n", line("a")).as_bytes()).unwrap();
        assert_eq!(reader.poll().unwrap().options.colors_name, "a");

        let mut bad = UnixStream::connect(feed.socket_path()).unwrap();
        bad.write_all(b"garbage\n").unwrap();
        assert!(reader.poll().is_none(), "a malformed payload is not news");

        let mut good_again = UnixStream::connect(feed.socket_path()).unwrap();
        good_again.write_all(format!("{}\n", line("b")).as_bytes()).unwrap();
        assert_eq!(reader.poll().unwrap().options.colors_name, "b");
        feed.cleanup();
    }

    #[test]
    fn a_feed_owns_a_private_directory_with_its_socket_and_snippet() {
        let feed = ThemeFeed::new().expect("feed");
        let env: std::collections::HashMap<_, _> = feed.child_env().into_iter().collect();
        let socket = std::path::PathBuf::from(&env["EITRI_THEME_SOCKET"]);
        let lua = std::path::PathBuf::from(&env["EITRI_THEME_LUA"]);
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

    /// Four connections that never write anything used to cost this feed's
    /// old accept loop the full per-syscall `READ_TIMEOUT` (100ms) each, one after another, because
    /// it put every accepted stream back into BLOCKING mode and `read_line`'d it in turn -- ~400ms
    /// total for four, and unboundedly more for a writer that trickles bytes slower than the
    /// timeout. `NewestLineReader` never blocks the caller at all.
    #[test]
    fn four_silent_connections_do_not_block_a_poll() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let clients: Vec<_> = (0..4)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        assert!(reader.poll().is_none());
        use std::os::fd::AsRawFd;
        let pending: Vec<_> = reader.0.pending_streams().collect();
        assert_eq!(pending.len(), 4, "every silent peer remains pending");
        for stream in pending {
            // SAFETY: the borrowed stream owns a live descriptor.
            let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
            assert!(
                flags >= 0 && flags & libc::O_NONBLOCK != 0,
                "a silent peer could block a poll"
            );
        }
        drop(clients);
        feed.cleanup();
    }

    #[test]
    fn a_single_poll_picks_the_newest_of_several_already_written_lines() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        for name in ["one", "two"] {
            let mut stream = UnixStream::connect(feed.socket_path()).expect("connect");
            stream.write_all(format!("{}\n", line(name)).as_bytes()).unwrap();
        }
        assert_eq!(reader.poll().unwrap().options.colors_name, "two");
        feed.cleanup();
    }

    /// The same reader's size bound: there used to be no cap at all on an unterminated line's size, so a
    /// contiguous multi-megabyte line with no newline was read and parsed in full. `NewestLineReader`
    /// closes an oversized connection instead, without discarding an earlier good payload or blocking
    /// a later good connection.
    #[test]
    fn an_oversized_unterminated_line_is_dropped_rather_than_read_whole() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).expect("connect");
        // Chunked with a poll between writes, so this does not depend on the OS socket send-buffer
        // capacity; no newline is ever sent, so the size bound must apply to a partial line too.
        for _ in 0..crate::line_feed::MAX_LINE_BYTES / 4096 {
            client.write_all(&[b'x'; 4096]).unwrap();
            assert!(reader.poll().is_none());
        }
        client.write_all(b"x").unwrap();
        assert!(
            reader.poll().is_none(),
            "an oversized unterminated line must not be read whole"
        );

        let mut good = UnixStream::connect(feed.socket_path()).unwrap();
        good.write_all(format!("{}\n", line("after-limit")).as_bytes()).unwrap();
        assert_eq!(reader.poll().unwrap().options.colors_name, "after-limit");
        feed.cleanup();
    }

    /// The trap a prefix rename sets: the sweep finds candidates **by prefix**, so a directory
    /// written under the old prefix stops being a candidate the moment the prefix changes -- it
    /// would sit under `TMPDIR` forever with nothing left in the code that names it. Sweeping
    /// [`SWEPT_NAMES`] rather than one pair is what closes that.
    #[test]
    fn the_sweep_still_reclaims_a_directory_under_the_old_prefix() {
        use std::os::unix::fs::PermissionsExt;
        // Own root, so this never touches a real window's directory and never races a sibling test.
        let root = std::env::temp_dir().join(format!("nv-th-sweeptest-{}", uuid::Uuid::new_v4().simple()));
        // pid 0 is rejected before any syscall by `pid_is_alive`, and `std::process::id()` never
        // returns it, so it is always a safe "definitely dead" pid to plant a fixture at.
        let current = root.join(format!("{DIR_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        // Exactly what a build before the prefix rename wrote: the old prefix and a hyphenated uuid.
        let legacy = root.join(format!("neovibe-theme-0-{}", uuid::Uuid::new_v4()));
        for dir in [&current, &legacy] {
            std::fs::create_dir_all(dir).expect("build the fixture");
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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

    /// The socket path at macOS's worst-case temporary directory is exactly 100 bytes.
    ///
    /// An inequality against `MAX_SOCKET_PATH_BYTES` would let growth from 100 to 103 bytes pass
    /// while silently consuming the remaining headroom. Pinning the exact length forces any
    /// increase to be deliberate, with a shorter directory or socket name if more margin is needed.
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
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn a_real_nvim_pushes_its_colorscheme() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
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

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            seen = reader.poll().filter(|p| p.options.colors_name == "retrobox");
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
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn a_busy_nvim_main_loop_does_not_lose_a_payload() {
        // The later VimEnter handler holds the loop; only a synchronous send can precede it.
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let held = feed.dir.join("held");
        let fifo = feed.dir.join("release");
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: the path is NUL-terminated and remains live for the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let handler = format!(
            "autocmd VimEnter * lua vim.fn.writefile({{'held'}}, '{}'); vim.fn.readfile('{}')",
            held.display(),
            fifo.display()
        );
        let mut child = std::process::Command::new("nvim")
            .args(["--headless", "--clean"])
            .args(feed.nvim_args())
            .args(["-c", &handler, "-c", "colorscheme retrobox"])
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");

        let deadline = Instant::now() + Duration::from_secs(60);
        while !held.exists() {
            assert!(Instant::now() < deadline, "nvim never entered the later handler");
            std::thread::yield_now();
        }
        let mut seen = None;
        // Polled at POLL_INTERVAL, exactly as `EditorFeeds` polls it from the host's timer.
        while Instant::now() < deadline && seen.is_none() {
            seen = reader.poll().filter(|p| p.options.colors_name == "retrobox");
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();

        assert!(seen.is_some(), "the busy main loop must not lose the payload entirely");
    }

    /// What `Ctrl+0` returns to is what the config left at VimEnter: `--cmd` runs before it, `-c` after the config and
    /// before VimEnter, so the second write here is a config's, and a `:let` after VimEnter is not.
    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn the_start_scale_is_the_configs_and_a_later_let_does_not_move_it() {
        for (config, want) in [
            (Some("let g:neovide_scale_factor = 1.25"), 1.25),
            (None, 1.0),
            (Some("let g:neovide_scale_factor = 'big'"), f64::NAN),
        ] {
            let mut feed = ThemeFeed::new().expect("feed");
            let mut reader = reader_for(&mut feed);
            let mut command = std::process::Command::new("nvim");
            command.args(["--headless", "--clean"]).args(feed.nvim_args());
            if let Some(config) = config {
                command.args(["-c", config]);
            }
            let mut child = command
                .args([
                    "-c",
                    "autocmd VimEnter * ++nested lua vim.g.neovide_scale_factor = 2.0; vim.cmd.colorscheme('retrobox')",
                ])
                .envs(feed.child_env())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("nvim on PATH");
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut seen = None;
            while Instant::now() < deadline && seen.is_none() {
                seen = reader.poll().filter(|p| p.options.colors_name == "retrobox");
                std::thread::sleep(POLL_INTERVAL);
            }
            let _ = child.kill();
            let _ = child.wait();
            feed.cleanup();
            let payload = seen.expect("the payload after the later write");
            match want {
                want if want.is_nan() => assert_eq!(payload.options.start_scale, None, "{config:?}"),
                want => assert_eq!(payload.options.start_scale, Some(want), "{config:?}"),
            }
        }
    }

    /// Starts a headless nvim with the feed loaded and `commands` run (`-c`, before VimEnter), and returns the first
    /// payload `accept` takes, or panics after ten seconds.
    fn payload_from_nvim(commands: &[&str], accept: impl Fn(&NvimThemePayload) -> bool) -> NvimThemePayload {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let mut command = std::process::Command::new("nvim");
        command.args(["--headless", "--clean"]).args(feed.nvim_args());
        for cmd in commands {
            command.args(["-c", cmd]);
        }
        let mut child = command
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            seen = reader.poll().filter(&accept);
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();
        seen.expect("nvim never pushed the awaited payload")
    }

    /// The Normal-mode cursor nvim reports once `guicursor` is `set` and the colorscheme has loaded.
    fn cursor_for(guicursor: &str) -> CursorInfo {
        let set = format!("set guicursor={guicursor}");
        let payload = payload_from_nvim(&[&set, "colorscheme retrobox"], |p| p.options.colors_name == "retrobox");
        payload.cursor.expect("the snippet always sends a cursor")
    }

    /// Like `cursor_for`, but `guicursor` is assigned from Lua, so it may hold bytes `:set` would not carry: `tail` is
    /// a Lua expression appended to `"n:block-"`.
    fn cursor_for_lua_tail(tail: &str) -> CursorInfo {
        let set = format!("lua vim.o.guicursor = 'n:block-' .. {tail}");
        let payload = payload_from_nvim(&[&set, "colorscheme retrobox"], |p| p.options.colors_name == "retrobox");
        payload.cursor.expect("the snippet always sends a cursor")
    }

    fn steady() -> (u32, u32, u32) {
        (0, 0, 0)
    }

    fn blink(cursor: &CursorInfo) -> (u32, u32, u32) {
        (cursor.blinkwait, cursor.blinkon, cursor.blinkoff)
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_default_is_steady_with_no_group() {
        let payload = payload_from_nvim(&["colorscheme retrobox"], |p| p.options.colors_name == "retrobox");
        let cursor = payload.cursor.expect("the snippet always sends a cursor");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (None, steady()));
        assert_eq!((cursor.fg, cursor.bg), (None, None));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_vim_classic() {
        let cursor = cursor_for("n-v-c:block-Cursor/lCursor");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (Some("Cursor"), steady()));
        assert!(
            cursor.fg.is_some() || cursor.bg.is_some(),
            "the Cursor group's colours: {cursor:?}"
        );
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_blink_numbers() {
        let cursor = cursor_for("n:block-blinkwait700-blinkon400-blinkoff250");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (None, (700, 400, 250)));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_blinkon0_wins() {
        let cursor = cursor_for("n:blinkwait700-blinkon400-blinkoff250,a:blinkon0");
        assert_eq!(blink(&cursor), (700, 0, 250));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_naming_a_missing_group() {
        let cursor = cursor_for("n:block-NoSuchGroup");
        assert_eq!(cursor.group.as_deref(), Some("NoSuchGroup"));
        assert_eq!((cursor.fg, cursor.bg), (None, None));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_modes_and_keywords_are_case_insensitive() {
        // nvim reads `N`, `BLOCK` and `BLINKWAIT` like their lowercase forms; the group keeps its own spelling.
        let cursor = cursor_for("N:BLOCK-Cursor");
        assert_eq!(cursor.group.as_deref(), Some("Cursor"));
        let cursor = cursor_for("n:BLINKWAIT700-BLINKON400-BLINKOFF250");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (None, (700, 400, 250)));
        let cursor = cursor_for("A:Block-VER25-cursor,N:blinkWait100");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (Some("cursor"), (100, 0, 0)));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_group_name_nvim_refuses_is_not_sent() {
        // `:set` accepts a name with a stray byte (nvim only complains), so it stays in the option; sending it would make
        // the host reject the whole frame as invalid UTF-8 and lose every later theme update. The payload arrives, with
        // no group, and a refused name also clears a valid group an earlier entry set, as it does in nvim.
        for tail in ["string.char(255)", "'Cu$r'", "'caf\\xc3\\xa9'"] {
            let cursor = cursor_for_lua_tail(tail);
            assert_eq!(cursor.group, None, "{tail}");
        }
        let set = "lua vim.o.guicursor = 'n:block-Cursor,n:block-' .. string.char(255)";
        let payload = payload_from_nvim(&[set, "colorscheme retrobox"], |p| p.options.colors_name == "retrobox");
        assert_eq!(payload.cursor.and_then(|c| c.group), None);
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_group_name_over_nvims_limit_is_not_sent() {
        // nvim keeps an option whose group name is over 200 bytes (E1249) and draws no group; a name this long in the
        // frame would take it past the feed's size limit and every later theme frame with it, so the payload must
        // arrive (the colorscheme it waits for comes after the option) and carry no group.
        assert_eq!(
            cursor_for_lua_tail("string.rep('a', 200)").group.as_deref(),
            Some("a".repeat(200).as_str())
        );
        for count in [201, 270_000] {
            let cursor = cursor_for_lua_tail(&format!("string.rep('a', {count})"));
            assert_eq!(cursor.group, None, "{count} bytes");
        }
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_shape_keyword_is_read_even_when_the_argument_goes_on() {
        // nvim reads `block`, `ver<n>`, `hor<n>` and the blink keywords off the front of an argument and takes what is
        // left as the group: `BlockCursor` is `block` and `Cursor`, never a group called `BlockCursor`.
        for (set, group) in [
            ("n:BlockCursor", Some("Cursor")),
            ("n:blockFoo/Bar", Some("Foo")),
            ("n:ver25x", Some("x")),
            ("n:HOR30Foo", Some("Foo")),
            ("n:Block", None),
            ("n:Foo-block", Some("Foo")),
            ("n:blinkx", Some("blinkx")),
            ("n:block--Foo", Some("Foo")),
            ("n:Foo--", None),
        ] {
            assert_eq!(cursor_for(set).group.as_deref(), group, "{set}");
        }
        let cursor = cursor_for("n:blinkwait7Foo-blinkon5blinkoff6");
        assert_eq!((cursor.group.as_deref(), blink(&cursor)), (Some("Foo"), (7, 5, 6)));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_blink_time_nvim_cannot_hold_reads_zero() {
        // nvim keeps the option but reads a number over 2147483647 (what an int holds) as 0, so its editor does not
        // blink there; the page must not be told 2147483647 (99999 ms) and blink while the editor is steady.
        for set in [
            "n:blinkwait2147483648-blinkon5",
            "n:blinkwait4294967296-blinkon5",
            "n:blinkwait99999999999999999999-blinkon5",
        ] {
            assert_eq!(blink(&cursor_for(set)), (0, 5, 0), "{set}");
        }
        assert_eq!(blink(&cursor_for("n:blinkwait2147483647")), (2_147_483_647, 0, 0));
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_a_group_name_with_dots_and_at_signs_is_sent() {
        let cursor = cursor_for_lua_tail("'My.Group@x'");
        assert_eq!(cursor.group.as_deref(), Some("My.Group@x"));
    }

    /// Only the `OptionSet guicursor` handler can deliver this: the option holds the group for 1.5 seconds and is then
    /// back at its default, and the periodic resnapshot (which sends what it sees at each tick, three seconds apart)
    /// never sees the group at all.
    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn guicursor_change_resends() {
        // Nested, so the `:set` inside the handler fires OptionSet; the payload at VimEnter still has no group.
        let handler = "autocmd VimEnter * ++nested set guicursor=n:block-Cursor | \
                       lua vim.defer_fn(function() vim.cmd('set guicursor&') end, 1500)";
        let payload = payload_from_nvim(&[handler], |p| {
            p.cursor.as_ref().is_some_and(|c| c.group.as_deref() == Some("Cursor"))
        });
        assert_eq!(payload.cursor.and_then(|c| c.group).as_deref(), Some("Cursor"));
    }

    /// Nvim has no general highlight-change event. A plugin autocmd registered after the loader
    /// can override a highlight after the initial snapshot. Here a second `++once VimEnter`
    /// handler changes `Normal.fg` after `colorscheme retrobox`; periodic snapshots must detect it
    /// within a bounded window instead of leaving the panel with the original color indefinitely.
    #[test]
    #[ignore = "spawns a real nvim, and sleeps past the resnapshot interval; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn a_late_highlight_override_is_eventually_caught_by_the_periodic_resnapshot() {
        let mut feed = ThemeFeed::new().expect("feed");
        let mut reader = reader_for(&mut feed);
        let mut child = std::process::Command::new("nvim")
            .args(["--headless", "--clean"])
            .args(feed.nvim_args())
            .args([
                "-c",
                "colorscheme retrobox",
                "-c",
                "autocmd VimEnter * ++once lua vim.api.nvim_set_hl(0, 'Normal', {fg = 0x123456})",
            ])
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            seen = reader
                .poll()
                .filter(|p| p.groups.get("Normal").and_then(|a| a.fg) == Some(0x123456));
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();

        assert!(
            seen.is_some(),
            "a highlight group changed after startup was never re-sent to the panel"
        );
    }
    /// Periodic snapshots must not resend unchanged theme data. Every received payload restyles
    /// the window, rethemes the WebView and queues editor and terminal renders on the GTK thread;
    /// the shell does not deduplicate. An unconditional timer did that every 3s in an idle window
    /// (connections at 0.01/3.01/6.01/9.01s, one distinct payload), so no new connection may arrive
    /// after startup when the theme stays unchanged.
    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p eitri-core theme::feed -- --ignored"]
    fn an_unchanged_theme_is_not_resent_by_the_periodic_resnapshot() {
        let mut feed = ThemeFeed::new().expect("feed");
        let listener = feed.take_listener().expect("listener");
        let mut nvim = super::embed_client::Embed::start(&feed.dir, &[], &[]);
        nvim.manual_timers();
        nvim.lua(
            "local src, opts = ...; _G.theme_teardown = assert(loadstring(src))(opts)",
            vec![
                NVIM_THEME_LUA.into(),
                rmpv::Value::Map(vec![("socket".into(), feed.socket_path().to_str().unwrap().into())]),
            ],
        );
        nvim.command("colorscheme retrobox");
        nvim.flush_scheduled();
        let drain = || {
            let mut arrivals = Vec::new();
            while let Ok((mut stream, _)) = listener.accept() {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stream, &mut text).unwrap();
                arrivals.push(text);
            }
            arrivals
        };
        let initial = drain();
        assert!(!initial.is_empty(), "nvim never sent its startup payload");
        for _ in 0..3 {
            nvim.advance_timers(3000);
            assert!(
                drain().is_empty(),
                "an unchanged periodic snapshot sent another payload"
            );
        }
        nvim.quit();
        feed.cleanup();
    }
}

#[cfg(test)]
#[path = "../../tests/support/embed_client.rs"]
mod embed_client;
