//! The transport half of wire 1: one Unix socket, nvim writing, the host polling.
//!
//! Deliberately the same shape as `crate::theme::feed` -- same instance directory scheme, same
//! `--cmd` loader, same non-blocking listener drained from the host's main loop -- because that
//! protocol is in production and its failure modes are already paid for. Read that module's
//! comments before changing anything here; several of them are about bugs, not style.

use std::io::{ErrorKind, Read};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::editor_context::compose::{EditorContext, Selection};

/// Instance-directory prefix. Six bytes, like the other two, and that is a budget rather than a
/// convention: `crate::instance_dir` builds `<tmp>/<prefix><pid>-<uuid32>`, which reaches 93 bytes
/// at the macOS worst case, against `agent::socket_path`'s 103-byte cap. Both existing sockets land
/// at exactly 100.
pub(crate) const DIR_PREFIX: &str = "nv-ec-";

/// The socket file inside that directory. Six bytes, for the reason above.
const SOCKET_NAME: &str = "e.sock";

/// The snippet's file name. Not shortened: it is `dofile`d and named by an environment variable,
/// never bound, so no `sockaddr_un` limit applies to it.
const LUA_NAME: &str = "editor_context.lua";

/// Every `(directory prefix, socket file)` pair this module has used. One row today; the second
/// exists the moment either name changes, because the sweep matches by prefix and a rename with no
/// second row orphans every pre-rename directory permanently.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME)];

/// How often the host should drain. The snippet already rate-limits itself to one write per 150ms,
/// so a shorter poll here buys nothing; a longer one would make a selection made just before the
/// user clicks the panel arrive late.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

// A stalled/oversized sender must not monopolize the GTK driver. The snippet normally sends a
// small update every 150ms; these limits also leave room for its bounded 400-line selection.
const MAX_PENDING_CONNECTIONS: usize = 16;
const MAX_ACCEPTS_PER_POLL: usize = 16;
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_READ_BYTES_PER_POLL: usize = 64 * 1024;
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(2);

pub(crate) const NVIM_EDITOR_CONTEXT_LUA: &str = include_str!("nvim_editor_context.lua");

/// The one `--cmd`. It checks the path before `dofile`, because `dofile(nil)` reads stdin -- which
/// under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken snippet costs the
/// context, never the editor.
pub(crate) const LOADER_CMD: &str =
    "lua local p = vim.env.NEOVIBE_EDITOR_LUA; if p and p ~= '' then pcall(dofile, p) end";

pub struct EditorContextFeed {
    dir: PathBuf,
    socket_path: PathBuf,
    lua_path: PathBuf,
    listener: Option<UnixListener>,
}

impl EditorContextFeed {
    /// Sweeps stale directories, builds this one, writes the snippet and binds the socket -- or
    /// returns `None` having logged why. `None` is supported and costs exactly one feature: nvim
    /// gets no extra env or args, and turns are sent with no editor context, as they are today.
    pub fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "editor-context");
        }

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: whatever can connect to this socket can tell the agent what the user is looking at.
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            eprintln!(
                "[editor-context] could not create {}: {e} -- turns will carry no editor context",
                dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[editor-context] {what} -- turns will carry no editor context");
            let _ = std::fs::remove_dir_all(&dir);
        };

        let lua_path = dir.join(LUA_NAME);
        if let Err(e) = std::fs::write(&lua_path, NVIM_EDITOR_CONTEXT_LUA) {
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
            fail(format!("could not make the editor-context socket non-blocking: {e}"));
            return None;
        }
        println!("[editor-context] feed at {}", socket_path.display());
        Some(EditorContextFeed {
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
                "NEOVIBE_EDITOR_SOCKET".to_string(),
                self.socket_path.display().to_string(),
            ),
            ("NEOVIBE_EDITOR_LUA".to_string(), self.lua_path.display().to_string()),
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

impl Drop for EditorContextFeed {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// A non-blocking reader retained across the host's existing 100ms polls.
///
/// Connecting and writing are separate async operations in the Lua sender. An accepted socket
/// need not contain a whole line yet: retaining it on `WouldBlock` avoids both the former GTK
/// thread's 100ms read wait and the lost updates a stateless non-blocking read would cause.
///
/// Accept order defines freshness, as it did in the original protocol. A newer complete, valid
/// update supersedes all older connections, even if an older sender only finishes on a later
/// poll. A malformed newer update does not discard the last valid context.
pub struct EditorContextReader {
    listener: UnixListener,
    pending: Vec<PendingContext>,
    next_sequence: u64,
    published_sequence: u64,
}

struct PendingContext {
    stream: UnixStream,
    bytes: Vec<u8>,
    accepted_at: Instant,
    sequence: u64,
}

enum ReadStatus {
    Pending,
    Complete(Vec<u8>),
    Closed,
}

impl PendingContext {
    fn read_available(&mut self, budget: &mut usize) -> ReadStatus {
        let mut buffer = [0; 4096];
        while *budget > 0 {
            let limit = buffer.len().min(*budget);
            match self.stream.read(&mut buffer[..limit]) {
                // Preserve read_line's EOF framing: a complete JSON value without a newline is
                // still valid; a truncated value is rejected by parse, without clearing the cache.
                Ok(0) => return ReadStatus::Complete(std::mem::take(&mut self.bytes)),
                Ok(read) => {
                    *budget -= read;
                    let newline = buffer[..read].iter().position(|byte| *byte == b'\n');
                    let length = newline.unwrap_or(read);
                    if self.bytes.len() + length > MAX_LINE_BYTES {
                        return ReadStatus::Closed;
                    }
                    self.bytes.extend_from_slice(&buffer[..length]);
                    if newline.is_some() {
                        return ReadStatus::Complete(std::mem::take(&mut self.bytes));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                    return ReadStatus::Pending;
                }
                Err(_) => return ReadStatus::Closed,
            }
        }
        ReadStatus::Pending
    }
}

impl EditorContextReader {
    /// Takes the feed's already non-blocking listener. Each accepted stream is explicitly made
    /// non-blocking too: Linux does not inherit the listener's flag, while macOS does.
    pub fn new(listener: UnixListener) -> Self {
        Self {
            listener,
            pending: Vec::new(),
            next_sequence: 1,
            published_sequence: 0,
        }
    }

    /// Returns the newest valid update available without waiting, or `None` for no news.
    pub fn poll(&mut self) -> Option<EditorContext> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Option<EditorContext> {
        self.pending
            .retain(|pending| now.duration_since(pending.accepted_at) < MAX_CONNECTION_AGE);
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!("[editor-context] could not make an accepted connection non-blocking: {e}");
                        continue;
                    }
                    if self.pending.len() == MAX_PENDING_CONNECTIONS {
                        // Prefer a recent context over a sender that has not completed an older one.
                        self.pending.remove(0);
                    }
                    self.pending.push(PendingContext {
                        stream,
                        bytes: Vec::new(),
                        accepted_at: now,
                        sequence: self.next_sequence,
                    });
                    self.next_sequence += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                Err(e) => {
                    eprintln!("[editor-context] accept failed: {e}");
                    break;
                }
            }
        }

        let mut budget = MAX_READ_BYTES_PER_POLL;
        let mut latest = None;
        // Newest first: an older large selection must not consume the read budget before a small,
        // recent cursor update. Completed malformed updates still leave earlier valid ones eligible.
        for index in (0..self.pending.len()).rev() {
            let pending = &mut self.pending[index];
            if pending.sequence <= self.published_sequence {
                self.pending.remove(index);
                continue;
            }
            match pending.read_available(&mut budget) {
                ReadStatus::Pending => {}
                ReadStatus::Closed => {
                    self.pending.remove(index);
                }
                ReadStatus::Complete(bytes) => {
                    let sequence = pending.sequence;
                    self.pending.remove(index);
                    match String::from_utf8(bytes)
                        .map_err(|e| e.to_string())
                        .and_then(|line| parse(&line))
                    {
                        Ok(context) => {
                            self.published_sequence = sequence;
                            latest = Some(context);
                        }
                        Err(e) => eprintln!("[editor-context] ignoring an update: {e}"),
                    }
                }
            }
        }
        latest
    }
}

/// The last line that parses. A malformed line is logged and skipped; it never discards an earlier
/// good update, and it never applies partially.
///
/// Returns `None` when no line parsed, which the caller must read as "no news", never as "no
/// context" -- the host keeps the last known state rather than forgetting where the user is because
/// one tick brought nothing.
pub fn latest_context(lines: Vec<String>) -> Option<EditorContext> {
    let mut latest = None;
    for line in lines {
        match parse(&line) {
            Ok(context) => latest = Some(context),
            Err(e) => eprintln!("[editor-context] ignoring an update: {e}"),
        }
    }
    latest
}

fn parse(line: &str) -> Result<EditorContext, String> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).map_err(|e| format!("not JSON: {e}"))?;
    let version = value.get("v").and_then(serde_json::Value::as_u64);
    if version != Some(1) {
        return Err(format!("unknown payload version {version:?}; this client speaks 1"));
    }
    let file = value
        .get("file")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let selection = match value.get("selection") {
        Some(serde_json::Value::Null) | None => None,
        Some(selection) => {
            let start_line = selection.get("start_line").and_then(serde_json::Value::as_u64);
            let end_line = selection.get("end_line").and_then(serde_json::Value::as_u64);
            let text = selection.get("text").and_then(serde_json::Value::as_str);
            match (start_line, end_line, text) {
                (Some(start_line), Some(end_line), Some(text)) => Some(Selection {
                    start_line: start_line as u32,
                    end_line: end_line as u32,
                    text: text.to_string(),
                }),
                // A selection object missing a field is dropped whole rather than half-applied: a
                // range with no text is exactly the unfalsifiable coordinate this wire exists to
                // avoid sending.
                _ => return Err("a selection must carry start_line, end_line and text".to_string()),
            }
        }
    };
    Ok(EditorContext { file, selection })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn an_incomplete_line_survives_until_a_later_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/partial"#).unwrap();
        assert!(reader.poll().is_none());
        client
            .write_all(b".rs\"}\n")
            .expect("an incomplete update must stay connected");
        let context = reader.poll().expect("retain the first part");
        assert_eq!(context.file, "/partial.rs");
    }

    /// A coarse guard against the old four successive 100ms socket waits. This is a real Unix
    /// socket check, not a claim about GUI frame time or input-to-photon latency.
    #[test]
    fn four_silent_connections_do_not_hold_the_ui_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let clients: Vec<_> = (0..4)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        let before = Instant::now();
        assert!(reader.poll().is_none());
        let elapsed = before.elapsed();
        eprintln!("editor-context poll with four silent clients: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(200), "poll blocked for {elapsed:?}");
        drop(clients);
    }

    #[test]
    fn a_late_old_connection_cannot_overwrite_a_newer_context() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/old"#).unwrap();
        assert!(reader.poll().is_none());

        let mut new = UnixStream::connect(feed.socket_path()).unwrap();
        new.write_all(b"{\"v\":1,\"file\":\"/new.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/new.rs");
        // Superseded connections may already have been closed; either way they cannot publish.
        let _ = old.write_all(b".rs\"}\n");
        assert!(reader.poll().is_none());
    }

    #[test]
    fn a_malformed_new_connection_does_not_supersede_an_older_valid_one() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/valid"#).unwrap();
        assert!(reader.poll().is_none());
        let mut new = UnixStream::connect(feed.socket_path()).unwrap();
        new.write_all(b"broken json\n").unwrap();
        assert!(reader.poll().is_none());
        old.write_all(b".rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/valid.rs");
    }

    #[test]
    fn split_utf8_is_decoded_only_after_the_line_finishes() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(b"{\"v\":1,\"file\":\"/\xe4").unwrap();
        assert!(reader.poll().is_none());
        client.write_all(b"\xb8\xad.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/中.rs");
    }

    #[test]
    fn closed_partial_messages_are_discarded_but_valid_eof_framing_still_works() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/partial"#).unwrap();
        assert!(reader.poll().is_none());
        drop(client);
        assert!(reader.poll().is_none());
        assert!(reader.pending.is_empty());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/eof.rs"}"#).unwrap();
        drop(client);
        assert_eq!(reader.poll().unwrap().file, "/eof.rs");
    }

    #[test]
    fn incomplete_connections_expire_without_a_real_time_sleep() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/expired"#).unwrap();
        let accepted = Instant::now();
        assert!(reader.poll_at(accepted).is_none());
        assert!(reader.poll_at(accepted + MAX_CONNECTION_AGE).is_none());
        let _ = client.write_all(b".rs\"}\n");
        assert!(reader.poll_at(accepted + MAX_CONNECTION_AGE).is_none());
        assert!(reader.pending.is_empty());
    }

    #[test]
    fn connection_pressure_evicts_the_oldest_incomplete_message() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/evicted"#).unwrap();
        assert!(reader.poll().is_none());
        let clients: Vec<_> = (0..MAX_PENDING_CONNECTIONS)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        assert!(reader.poll().is_none());
        let _ = old.write_all(b".rs\"}\n");
        assert!(reader.poll().is_none());
        drop(clients);
    }

    #[test]
    fn accepting_a_backlog_is_bounded_per_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut clients: Vec<_> = (0..=MAX_ACCEPTS_PER_POLL)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        clients
            .last_mut()
            .unwrap()
            .write_all(b"{\"v\":1,\"file\":\"/last.rs\"}\n")
            .unwrap();
        assert!(
            reader.poll().is_none(),
            "the accept budget leaves the last connection for the next poll"
        );
        assert_eq!(reader.poll().unwrap().file, "/last.rs");
    }

    #[test]
    fn the_read_budget_keeps_large_messages_for_later_polls() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        let file = "x".repeat(MAX_READ_BYTES_PER_POLL);
        writeln!(client, "{{\"v\":1,\"file\":\"{file}\"}}").unwrap();
        assert!(
            reader.poll().is_none(),
            "one poll must not consume more than its read budget"
        );
        assert_eq!(reader.poll().unwrap().file, file);
    }

    #[test]
    fn oversized_messages_are_closed_without_blocking_later_updates() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        // Stream in chunks with a poll between them, so this test does not depend on the OS socket
        // send-buffer capacity. No newline is sent; the size limit must apply to partial lines too.
        for _ in 0..MAX_LINE_BYTES / 4096 {
            client.write_all(&[b'x'; 4096]).unwrap();
            assert!(reader.poll().is_none());
        }
        client.write_all(b"x").unwrap();
        assert!(reader.poll().is_none());
        assert!(
            reader.pending.is_empty(),
            "an oversized incomplete line must be dropped"
        );
        let mut good = UnixStream::connect(feed.socket_path()).unwrap();
        good.write_all(b"{\"v\":1,\"file\":\"/after-limit.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/after-limit.rs");
    }

    #[test]
    fn a_real_snippet_line_parses_into_a_selection() {
        let line =
            r#"{"v":1,"file":"/p/a.rs","line":13,"selection":{"start_line":12,"end_line":14,"text":"fn a() {}"}}"#;
        let context = parse(line).unwrap();
        assert_eq!(context.file, "/p/a.rs");
        assert_eq!(
            context.selection,
            Some(Selection {
                start_line: 12,
                end_line: 14,
                text: "fn a() {}".into()
            })
        );
    }

    /// `vim.json.encode` writes a missing table field as an absent key, not as null, so both have to
    /// mean the same thing -- and both have to mean "no selection", never "an empty one".
    #[test]
    fn an_absent_or_null_selection_is_no_selection() {
        for line in [
            r#"{"v":1,"file":"/p/a.rs","line":1}"#,
            r#"{"v":1,"file":"/p/a.rs","line":1,"selection":null}"#,
        ] {
            assert_eq!(parse(line).unwrap().selection, None, "{line}");
        }
    }

    /// A range with no text is the unfalsifiable coordinate this wire exists to avoid: on an
    /// unsaved buffer nothing downstream can discover that the file on disk says something else.
    #[test]
    fn a_selection_missing_its_text_is_rejected_rather_than_sent_as_a_bare_range() {
        let line = r#"{"v":1,"file":"/p/a.rs","selection":{"start_line":1,"end_line":2}}"#;
        assert!(parse(line).is_err());
    }

    /// The version is checked, not ignored. A future snippet writing v2 must not be read as v1 by
    /// an old binary that happens to recognise some of its fields.
    #[test]
    fn an_unknown_payload_version_is_refused() {
        assert!(parse(r#"{"v":2,"file":"/p/a.rs"}"#).is_err());
        assert!(parse(r#"{"file":"/p/a.rs"}"#).is_err());
    }

    /// The last good line wins, and a malformed one in the middle destroys nothing.
    #[test]
    fn the_newest_parseable_update_wins_and_a_broken_line_is_skipped() {
        let lines = vec![
            r#"{"v":1,"file":"/first.rs"}"#.to_string(),
            "not json at all".to_string(),
            r#"{"v":1,"file":"/second.rs"}"#.to_string(),
        ];
        assert_eq!(latest_context(lines).unwrap().file, "/second.rs");
    }

    /// No lines means "no news". The caller keeps its last known state; returning a context here
    /// would let one empty tick erase where the user is.
    #[test]
    fn no_lines_is_no_news_rather_than_no_context() {
        assert!(latest_context(Vec::new()).is_none());
    }

    #[test]
    fn the_loader_checks_its_path_before_dofile() {
        assert!(LOADER_CMD.starts_with("lua "));
        assert!(LOADER_CMD.contains("if p and p ~= ''"));
        assert!(LOADER_CMD.contains("pcall(dofile, p)"));
    }

    /// The snippet and this parser are one contract in two languages; nothing makes the compiler
    /// check it. These pin the field names on the Lua side so a rename there fails here.
    #[test]
    fn the_snippet_writes_the_fields_this_parser_reads() {
        for needle in ["v = 1", "file =", "selection =", "start_line =", "end_line =", "text ="] {
            assert!(
                NVIM_EDITOR_CONTEXT_LUA.contains(needle),
                "the snippet no longer writes {needle:?}"
            );
        }
        for needle in ["NEOVIBE_EDITOR_SOCKET", "getregion", "getpos"] {
            assert!(
                NVIM_EDITOR_CONTEXT_LUA.contains(needle),
                "the snippet no longer uses {needle:?}"
            );
        }
        // The marks are the thing this wire must never read; see `selection()` in the snippet.
        assert!(
            !NVIM_EDITOR_CONTEXT_LUA.contains("getpos(\"'<\")"),
            "the snippet must never read the visual marks"
        );
    }

    /// The socket path at macOS's own worst case, asserted **exactly** rather than against the cap.
    ///
    /// `assert_eq!(…, 100)` and not `<= MAX_SOCKET_PATH_BYTES`, for the reason `theme::feed`'s twin
    /// gives: an inequality lets the whole margin be eaten in silence. This is the THIRD socket in
    /// this crate and all three now sit at exactly 100 against a 103-byte cap, so the remaining
    /// budget is three bytes for every future protocol combined -- which is why this wire's
    /// directory prefix and file name are both six bytes rather than something legible.
    #[test]
    fn the_editor_context_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        // `/var/folders/<2>/<28>/T/` -- this project's Mac mini, measured 2026-09-17.
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        // A synthetic 5-digit pid: macOS's PID_MAX is 99999, and the length under test must not
        // drift with whatever pid happens to run the suite.
        let dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{}", uuid::Uuid::new_v4().simple()));
        let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
        assert!(
            path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
            "{path:?}"
        );
    }

    #[test]
    fn a_real_feed_binds_and_round_trips_one_line() {
        let Some(mut feed) = EditorContextFeed::new() else {
            return;
        };
        let socket = feed.socket_path().to_path_buf();
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        {
            use std::io::Write;
            let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            writeln!(
                client,
                r#"{{"v":1,"file":"/p/b.rs","selection":{{"start_line":3,"end_line":3,"text":"x"}}}}"#
            )
            .unwrap();
        }
        let context = reader.poll().expect("the line must arrive");
        assert_eq!(context.file, "/p/b.rs");
        assert_eq!(context.selection.unwrap().start_line, 3);
        feed.cleanup();
    }
}
