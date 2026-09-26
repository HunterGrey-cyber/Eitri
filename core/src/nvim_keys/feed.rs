//! The transport half of the nvim report (spec `2026-09-26-panel-round2-design.md` §3): a fourth
//! nvim->host push socket, nvim writing one JSON line per changed report, the host polling.
//!
//! Deliberately a structural copy of `crate::editor_context::feed` -- same instance directory
//! scheme, same `--cmd` loader, same non-blocking listener drained from the host's main loop, and
//! the same bounded reader (`crate::line_feed`) -- because that protocol is in production and its
//! failure modes are already paid for. Read that module's comments before changing anything here.
//!
//! What crosses this socket is nvim-supplied bytes, so the reader treats it as untrusted: a line
//! that is not a v1 report ([`super::parse_report`]), or is larger than the line cap, is dropped
//! whole and the host keeps its last good report (spec §3.8).

use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{parse_report, NvimReport};
use crate::line_feed::NewestLineReader;

/// Instance-directory prefix. Six bytes, like the other three: `crate::instance_dir` builds
/// `<tmp>/<prefix><pid>-<uuid32>`, and every socket in this crate sits at exactly 100 bytes at the
/// macOS worst case against `agent::socket_path`'s 103-byte cap.
pub(crate) const DIR_PREFIX: &str = "nv-nk-";

/// The socket file inside that directory. Six bytes, for the reason above.
const SOCKET_NAME: &str = "k.sock";

/// The snippet's file name. Not shortened: it is `dofile`d and named by an environment variable,
/// never bound, so no `sockaddr_un` limit applies to it.
const LUA_NAME: &str = "nvim_keys.lua";

/// Every `(directory prefix, socket file)` pair this module has used. One row today; a rename adds
/// a second, because the sweep matches by prefix and a rename with no second row orphans every
/// pre-rename directory permanently.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME)];

/// How often the host should drain. The snippet debounces its own sends by 100ms and sends only a
/// changed report, so a shorter poll buys nothing.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) const NVIM_KEYS_LUA: &str = include_str!("nvim_keys.lua");

/// The one `--cmd`. It checks the path before `dofile`, because `dofile(nil)` reads stdin -- which
/// under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken snippet costs the
/// panel its nvim keys, never the editor.
pub(crate) const LOADER_CMD: &str =
    "lua local p = vim.env.NEOVIBE_KEYS_LUA; if p and p ~= '' then pcall(dofile, p) end";

pub struct NvimKeysFeed {
    dir: PathBuf,
    socket_path: PathBuf,
    lua_path: PathBuf,
    listener: Option<UnixListener>,
}

impl NvimKeysFeed {
    /// Sweeps stale directories, builds this one, writes the snippet and binds the socket -- or
    /// returns `None` having logged why. `None` is supported and costs exactly one feature: nvim
    /// gets no extra env or args, and the panel keeps its default keys.
    pub fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "nvim-keys");
        }

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: whatever can connect to this socket can tell the panel which keys do what.
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            eprintln!(
                "[nvim-keys] could not create {}: {e} -- the panel keeps its default keys",
                dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[nvim-keys] {what} -- the panel keeps its default keys");
            let _ = std::fs::remove_dir_all(&dir);
        };

        let lua_path = dir.join(LUA_NAME);
        if let Err(e) = std::fs::write(&lua_path, NVIM_KEYS_LUA) {
            fail(format!("could not write the nvim snippet: {e}"));
            return None;
        }
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
            fail(format!("could not make the nvim-keys socket non-blocking: {e}"));
            return None;
        }
        println!("[nvim-keys] feed at {}", socket_path.display());
        Some(NvimKeysFeed {
            dir,
            socket_path,
            lua_path,
            listener: Some(listener),
        })
    }

    /// Set on the nvim child only.
    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "NEOVIBE_KEYS_SOCKET".to_string(),
                self.socket_path.display().to_string(),
            ),
            ("NEOVIBE_KEYS_LUA".to_string(), self.lua_path.display().to_string()),
        ]
    }

    pub fn nvim_args(&self) -> Vec<String> {
        vec!["--cmd".to_string(), LOADER_CMD.to_string()]
    }

    /// Hands the bound listener to the host's polling driver. `None` the second time, which is how
    /// a host detects that it wired two drivers to one feed.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Removes the directory. Called explicitly from the host's close handler: GTK does not
    /// reliably drop signal-handler closures before the process exits, so `Drop` alone leaks.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for NvimKeysFeed {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// The nvim-keys reader: [`NewestLineReader`]'s bounded, newest-wins polling with
/// [`parse_report`] as its acceptance test. `None` is "no news", never "no keys": the caller keeps
/// its last good report (spec §3.8).
pub struct NvimKeysReader(NewestLineReader);

impl NvimKeysReader {
    pub fn new(listener: UnixListener) -> Self {
        Self(NewestLineReader::new(listener, "nvim-keys"))
    }

    /// Returns the newest valid report available without waiting, or `None` for no news.
    pub fn poll(&mut self) -> Option<NvimReport> {
        self.0
            .poll_with(|line| {
                let ok = parse_report(line).is_some();
                if !ok {
                    eprintln!("[nvim-keys] ignoring a report that is not v1 JSON");
                }
                ok
            })
            .and_then(|line| parse_report(&line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line_feed::MAX_READ_BYTES_PER_POLL;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    fn bound_pair() -> (NvimKeysFeed, NvimKeysReader) {
        let mut feed = NvimKeysFeed::new().expect("bind the test feed");
        let reader = NvimKeysReader::new(feed.take_listener().unwrap());
        (feed, reader)
    }

    /// Connects, writes `bytes` plus `\n`, and closes -- on its own thread, because a line larger
    /// than the socket's send buffer only drains as the reader polls.
    fn send_line(feed: &NvimKeysFeed, bytes: &[u8]) -> std::thread::JoinHandle<()> {
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        let mut payload = bytes.to_vec();
        payload.push(b'\n');
        std::thread::spawn(move || {
            // An oversized line is closed by the reader mid-write; the error is the point.
            let _ = client.write_all(&payload);
        })
    }

    #[test]
    fn the_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        // The twin of editor_context/feed.rs's test: the Mac mini's 49-byte TMPDIR, a 5-digit pid.
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        let dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{}", uuid::Uuid::new_v4().simple()));
        let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
    }

    #[test]
    fn a_malformed_or_oversized_line_is_dropped_and_the_last_good_report_stays() {
        let (feed, mut reader) = bound_pair();
        send_line(
            &feed,
            br#"{"v":1,"mapleader":null,"timeoutlen":300,"timeout":true,"maps":[]}"#,
        )
        .join()
        .unwrap();
        assert_eq!(reader.poll().unwrap().timeoutlen, 300);
        send_line(&feed, b"{broken").join().unwrap();
        // A valid-looking report padded past the line cap: this must fail on the size cap itself
        // (`_pad` alone puts the payload over MAX_LINE_BYTES), not merely be unparseable -- the
        // cap's own coverage lives in `line_feed`'s `oversized_messages_are_closed_without_blocking_later_updates`.
        let oversized_report = format!(
            r#"{{"v":1,"mapleader":null,"timeoutlen":300,"timeout":true,"maps":[],"_pad":"{}"}}"#,
            "x".repeat(300 * 1024)
        );
        let oversized = send_line(&feed, oversized_report.as_bytes());
        // Poll until the writer thread finishes, bounded so a regression fails fast instead of
        // hanging the suite. A fixed count of polls before `join()` can finish before the writer
        // has even been scheduled; then nothing drains the socket, the writer blocks forever on a
        // full send buffer and `join()` hangs (seen twice: an 11-minute wedge, and 2/20 runs).
        // The writer finishing proves nothing about the cap: it finishes as soon as its last bytes
        // land in the kernel's send buffer (~200KB on Linux), well before the reader has seen them.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !oversized.is_finished() {
            assert!(reader.poll().is_none(), "no news, not a reset");
            assert!(
                std::time::Instant::now() < deadline,
                "the writer thread never finished -- did the reader stop draining the connection?"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        oversized.join().unwrap();
        // So drain the rest: one poll reads at most MAX_READ_BYTES_PER_POLL, and the payload is
        // ~300KiB, so ceil(300/64)+1 = 6 polls reach the end of the line if nothing closed it (10 run).
        // The padded report parses (`NvimReport` ignores unknown fields), so only the size cap
        // can make every one of these `None` -- checked by replacing the cap test with `false`.
        let polls = (oversized_report.len() / MAX_READ_BYTES_PER_POLL) + 2;
        for _ in 0..polls.max(10) {
            assert!(
                reader.poll().is_none(),
                "the size cap must drop the oversized line whole"
            );
        }
    }

    #[test]
    fn an_unknown_wire_version_is_no_news() {
        let (feed, mut reader) = bound_pair();
        send_line(
            &feed,
            br#"{"v":2,"mapleader":null,"timeoutlen":300,"timeout":true,"maps":[]}"#,
        )
        .join()
        .unwrap();
        assert!(reader.poll().is_none());
    }

    #[test]
    fn the_loader_checks_its_path_before_dofile() {
        assert!(LOADER_CMD.starts_with("lua "));
        assert!(LOADER_CMD.contains("NEOVIBE_KEYS_LUA"));
        assert!(LOADER_CMD.contains("if p and p ~= ''"));
        assert!(LOADER_CMD.contains("pcall(dofile, p)"));
    }

    /// The snippet and `parse_report` are one contract in two languages; these pin the field names
    /// on the Lua side so a rename there fails here. The real-nvim test checks the values.
    #[test]
    fn the_snippet_writes_the_fields_the_report_reads() {
        for needle in [
            "v = 1",
            "mapleader =",
            "timeoutlen =",
            "timeout =",
            "maps =",
            "lhs =",
            "rhs =",
            "desc =",
            "callback =",
            "NEOVIBE_KEYS_SOCKET",
            "keytrans",
        ] {
            assert!(
                NVIM_KEYS_LUA.contains(needle),
                "the snippet no longer writes {needle:?}"
            );
        }
    }

    #[test]
    fn the_child_env_names_the_socket_and_the_snippet() {
        let (feed, _reader) = bound_pair();
        let env = feed.child_env();
        assert!(env
            .iter()
            .any(|(k, v)| k == "NEOVIBE_KEYS_SOCKET" && v.ends_with(&format!("/{SOCKET_NAME}"))));
        assert!(env
            .iter()
            .any(|(k, v)| k == "NEOVIBE_KEYS_LUA" && v.ends_with("/nvim_keys.lua")));
        assert_eq!(feed.nvim_args(), vec!["--cmd".to_string(), LOADER_CMD.to_string()]);
        feed.cleanup();
    }
}
