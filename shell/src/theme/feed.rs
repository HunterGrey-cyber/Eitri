//! The nvim → shell half of the theme: a per-window socket the embedded nvim writes snapshots to.
//!
//! Same direction and same main-loop poll as `pane_switch` -- nvim pushes, `shell` drains. Push
//! rather than pull because a colorscheme change happens inside nvim; `shell` has no way to know
//! when to ask. It never touches the RPC pipe the keyboard path uses, and needs no fork change.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

use gtk4::glib;

use super::payload::{parse_payload, NvimThemePayload};

const DIR_PREFIX: &str = "neovibe-theme-";
const SOCKET_NAME: &str = "theme.sock";
const LUA_NAME: &str = "theme.lua";

/// A colorscheme change is not latency-sensitive the way a keypress is; 100ms is imperceptible and
/// an idle poll of a non-blocking `accept()` costs one failing syscall.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on how long one accepted connection may block the GTK main loop. The snippet connects and
/// writes synchronously inside one autocommand (see `send` in `nvim_theme.lua`), so the line is
/// normally already in the socket when the connection is accepted; this only bounds a misbehaving
/// writer. An *accepted* `UnixStream` does not inherit its listener's non-blocking flag on Linux,
/// which is why this has to be set explicitly (see `pane_switch::READ_TIMEOUT`).
const READ_TIMEOUT: Duration = Duration::from_millis(100);

const NVIM_THEME_LUA: &str = include_str!("nvim_theme.lua");

/// The one `--cmd` handed to nvim. It checks the path before `dofile`, because `dofile(nil)` reads
/// stdin -- which under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken
/// snippet costs the theme, never the editor.
pub(crate) const LOADER_CMD: &str =
    "lua local p = vim.env.NEOVIBE_THEME_LUA; if p and p ~= '' then pcall(dofile, p) end";

pub(crate) struct ThemeFeed {
    dir: PathBuf,
    socket_path: PathBuf,
    lua_path: PathBuf,
    listener: Option<UnixListener>,
}

impl ThemeFeed {
    /// Builds the directory, writes the snippet and binds the socket, or returns `None` having
    /// logged why. `None` is supported: nvim gets no extra env or args, and the window stays on
    /// `ThemeTokens::fallback()`.
    pub(crate) fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        crate::instance_dir::sweep_stale_instance_dirs(&tmp, DIR_PREFIX, SOCKET_NAME, "theme");

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: the socket accepts colours from anyone who can connect to it.
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            eprintln!("[theme] could not create {}: {e} -- colours stay on the built-in fallback", dir.display());
            return None;
        }
        let fail = |what: &str, e: std::io::Error| {
            eprintln!("[theme] {what}: {e} -- colours stay on the built-in fallback");
            let _ = std::fs::remove_dir_all(&dir);
        };

        let lua_path = dir.join(LUA_NAME);
        if let Err(e) = std::fs::write(&lua_path, NVIM_THEME_LUA) {
            fail("could not write the nvim snippet", e);
            return None;
        }
        let socket_path = dir.join(SOCKET_NAME);
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                fail(&format!("could not bind {}", socket_path.display()), e);
                return None;
            }
        };
        if let Err(e) = listener.set_nonblocking(true) {
            fail("could not make the theme socket non-blocking", e);
            return None;
        }
        println!("[theme] feed at {}", socket_path.display());
        Some(ThemeFeed { dir, socket_path, lua_path, listener: Some(listener) })
    }

    /// Set on the nvim child only, through `NeovideEditorPaneOptions::child_env`.
    pub(crate) fn child_env(&self) -> Vec<(String, String)> {
        vec![
            ("NEOVIBE_THEME_SOCKET".to_string(), self.socket_path.display().to_string()),
            ("NEOVIBE_THEME_LUA".to_string(), self.lua_path.display().to_string()),
        ]
    }

    /// Passed through `NeovideEditorPaneOptions::extra_nvim_args`.
    pub(crate) fn nvim_args(&self) -> Vec<String> {
        vec!["--cmd".to_string(), LOADER_CMD.to_string()]
    }

    /// Polls the socket on the GTK main loop and calls `on_payload` with the newest valid payload
    /// each tick. `VimEnter` and `ColorScheme` often fire back to back; only the last one matters.
    /// Consumes the listener, so it can only be called once.
    pub(crate) fn listen(&mut self, on_payload: impl Fn(NvimThemePayload) + 'static) {
        let Some(listener) = self.listener.take() else {
            eprintln!("[theme] listen() called twice -- ignoring");
            return;
        };
        glib::timeout_add_local(POLL_INTERVAL, move || {
            if let Some(payload) = latest_payload(accept_pending_lines(&listener)) {
                on_payload(payload);
            }
            glib::ControlFlow::Continue
        });
    }

    /// Removes the directory. Called explicitly from the window's close handler, for the reason
    /// `PaneSwitch::cleanup` documents: GTK does not reliably drop signal-handler closures before
    /// the process exits, so `Drop` alone leaks.
    pub(crate) fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for ThemeFeed {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// One line per pending connection, in accept order.
fn accept_pending_lines(listener: &UnixListener) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
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
fn latest_payload(lines: Vec<String>) -> Option<NvimThemePayload> {
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
        format!(r#"{{"v":1,"groups":{{}},"options":{{"background":"dark","guifont":"","colors_name":"{colors_name}"}}}}"#)
    }

    #[test]
    fn the_lua_snippet_requests_every_group_derivation_reads() {
        for group in GROUPS_READ {
            assert!(NVIM_THEME_LUA.contains(&format!("\"{group}\"")), "nvim_theme.lua does not request {group}");
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
        assert_eq!(got.options.colors_name, "a", "a bad line never discards an earlier good one");
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
        assert_eq!(std::fs::metadata(&feed.dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(feed.nvim_args(), vec!["--cmd".to_string(), LOADER_CMD.to_string()]);
        feed.cleanup();
        assert!(!feed.dir.exists());
    }

    #[test]
    fn every_pending_connection_is_drained_in_one_pass() {
        let feed = ThemeFeed::new().expect("feed");
        for name in ["one", "two"] {
            let mut stream = UnixStream::connect(&feed.socket_path).expect("connect");
            stream.write_all(format!("{}\n", line(name)).as_bytes()).unwrap();
        }
        let lines = accept_pending_lines(feed.listener.as_ref().unwrap());
        assert_eq!(lines.len(), 2);
        assert_eq!(latest_payload(lines).unwrap().options.colors_name, "two");
        feed.cleanup();
    }

    #[test]
    #[ignore = "spawns a real nvim; run with: cargo test -p shell --bin shell theme::feed -- --ignored"]
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
    #[ignore = "spawns a real nvim; run with: cargo test -p shell --bin shell theme::feed -- --ignored"]
    fn a_busy_nvim_main_loop_does_not_lose_a_payload() {
        // A user's own VimEnter handler (session restore, a dashboard) runs after ours, which `--cmd`
        // registered first. A write that waits for nvim's loop to turn would arrive after this 400ms
        // handler -- past READ_TIMEOUT -- and the poll below would drop the connection.
        let feed = ThemeFeed::new().expect("feed");
        let mut child = std::process::Command::new("nvim")
            .args(["--headless", "--clean"])
            .args(feed.nvim_args())
            .args(["-c", "autocmd VimEnter * lua vim.uv.sleep(400)", "-c", "colorscheme retrobox"])
            .envs(feed.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");

        let listener = feed.listener.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = Vec::new();
        // Polled at POLL_INTERVAL, exactly as `listen` polls on the GTK main loop.
        while Instant::now() < deadline && received.len() < 2 {
            received.extend(accept_pending_lines(listener).iter().filter_map(|line| parse_payload(line).ok()));
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = child.kill();
        let _ = child.wait();
        feed.cleanup();

        assert!(received.len() >= 2, "expected the ColorScheme and the VimEnter payload, got {}", received.len());
        assert!(received.iter().all(|p| p.options.colors_name == "retrobox"));
    }
}
