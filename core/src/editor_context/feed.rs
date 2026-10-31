//! The transport half of wire 1: one Unix socket, nvim writing, the host polling.
//!
//! Like `crate::theme::feed`, it uses a private instance directory, a guarded `--cmd` loader and
//! a non-blocking listener drained from the host's main loop, so reading context never waits on nvim.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::editor_context::compose::{EditorContext, Selection};
use crate::line_feed::NewestLineReader;

/// Instance-directory prefix. Six bytes, like every other feed's, and that is a budget rather than a
/// convention: `crate::instance_dir` builds `<tmp>/<prefix><pid>-<uuid32>`, which reaches 93 bytes
/// at the macOS worst case, against `agent::socket_path`'s 103-byte cap. Every feed socket in this
/// crate lands at exactly 100.
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

pub(crate) const NVIM_EDITOR_CONTEXT_LUA: &str = include_str!("nvim_editor_context.lua");

/// The one `--cmd`. It checks the path before `dofile`, because `dofile(nil)` reads stdin -- which
/// under `--embed` is the RPC pipe -- and wraps the load in `pcall`, so a broken snippet costs the
/// context, never the editor.
pub(crate) const LOADER_CMD: &str =
    "lua local p = vim.env.EITRI_EDITOR_LUA; if p and p ~= '' then pcall(dofile, p) end";

/// What a selection push needs beyond [`EditorContext`], taken from the same line of the wire. Kept apart from it
/// because only the window with an IDE listener asks: every other reader of a context, and the copy a linked window
/// gets, stays as it was. Each field is absent on a line that does not carry it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushFacts {
    /// 1-based, as the wire's lines are.
    pub cursor_line: Option<u32>,
    /// In UTF-16 units from the start of the selection's first line.
    pub start_col: Option<u32>,
    /// In UTF-16 units, exclusive, from the start of its last line. 0 for a linewise selection and for one ending on
    /// an empty line.
    pub end_col: Option<u32>,
}

/// A context and the optional push facts read from the same wire line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedUpdate {
    pub context: EditorContext,
    pub facts: PushFacts,
}

pub struct EditorContextFeed {
    dir: PathBuf,
    socket_path: PathBuf,
    lua_path: PathBuf,
    listener: Option<UnixListener>,
}

impl EditorContextFeed {
    /// Sweeps stale directories, builds this one, writes the snippet and binds the socket -- or
    /// returns `None` having logged why. `None` is supported and costs exactly one feature: nvim
    /// gets no extra env or args, and turns are sent with no editor context.
    pub fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "editor-context");
        }

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: whatever can connect to this socket can tell the agent what the user is looking at.
        if let Err(e) = crate::instance_dir::create_private_dir(&dir) {
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
        if let Err(e) = crate::instance_dir::make_socket_private(&socket_path) {
            fail(format!(
                "could not make {} reachable by nvim: {e}",
                socket_path.display()
            ));
            return None;
        }
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
                "EITRI_EDITOR_SOCKET".to_string(),
                self.socket_path.display().to_string(),
            ),
            ("EITRI_EDITOR_LUA".to_string(), self.lua_path.display().to_string()),
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

/// The editor-context reader: [`crate::line_feed::NewestLineReader`]'s bounded, newest-wins
/// polling with this wire's parser as its acceptance test. A malformed newer update does not
/// discard the last valid context; the bounds and freshness rules are documented, and tested,
/// in `line_feed`.
pub struct EditorContextReader(NewestLineReader);

impl EditorContextReader {
    /// The bounded reader underneath, for a host deciding whether there is anything to read.
    pub(crate) fn lines(&self) -> &NewestLineReader {
        &self.0
    }

    pub fn new(listener: UnixListener) -> Self {
        Self(NewestLineReader::new(listener, "editor-context"))
    }

    /// Forgets every update not yet returned (see [`NewestLineReader::discard`]).
    pub fn discard(&mut self) {
        self.0.discard();
    }

    /// Returns the newest valid update available without waiting, or `None` for no news.
    pub fn poll(&mut self) -> Option<EditorContext> {
        self.poll_update().map(|update| update.context)
    }

    /// Returns the newest valid context together with its optional push facts, without waiting.
    pub fn poll_update(&mut self) -> Option<FeedUpdate> {
        self.0.poll_parsed(|line| match parse_bytes(line) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                eprintln!("[editor-context] ignoring an update: {e}");
                None
            }
        })
    }
}

/// A line that is not UTF-8 (nvim sends a file name's bytes as they are) is read with each invalid
/// sequence as one U+FFFD: the update still says which file is open, close enough for a person or an agent to know it,
/// where dropping it would leave the file open before it named as the current one.
fn parse_bytes(line: &[u8]) -> Result<FeedUpdate, String> {
    parse_update(&String::from_utf8_lossy(line))
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
    parse_update(line).map(|update| update.context)
}

fn parse_facts(value: &serde_json::Value) -> PushFacts {
    let column = |value: Option<&serde_json::Value>| value?.as_u64().and_then(|n| u32::try_from(n).ok());
    let selection = value.get("selection");
    PushFacts {
        cursor_line: column(value.get("line")),
        start_col: column(selection.and_then(|s| s.get("start_col"))),
        end_col: column(selection.and_then(|s| s.get("end_col"))),
    }
}

fn parse_update(line: &str) -> Result<FeedUpdate, String> {
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
    Ok(FeedUpdate {
        context: EditorContext { file, selection },
        facts: parse_facts(&value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_without_columns_stays_valid() {
        for (line, selection, cursor_line) in [
            (
                r#"{"v":1,"file":"/p/a.rs","line":13,"selection":{"start_line":12,"end_line":14,"text":"fn a() {}"}}"#,
                Some(Selection {
                    start_line: 12,
                    end_line: 14,
                    text: "fn a() {}".into(),
                }),
                Some(13),
            ),
            (r#"{"v":1,"file":"/p/a.rs","line":1,"selection":null}"#, None, Some(1)),
            (r#"{"v":1,"file":"/p/a.rs"}"#, None, None),
        ] {
            let update = parse_update(line).unwrap();
            assert_eq!(
                update.context,
                EditorContext {
                    file: "/p/a.rs".into(),
                    selection
                }
            );
            assert_eq!(
                update.facts,
                PushFacts {
                    cursor_line,
                    start_col: None,
                    end_col: None
                }
            );
        }
    }

    #[test]
    fn a_wrong_column_type_reads_as_absent() {
        for value in [
            serde_json::json!("1"),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!(4294967296_u64),
        ] {
            let line = serde_json::json!({"v": 1, "file": "/p/a.rs", "line": value,
                "selection": {"start_line": 1, "end_line": 1, "text": "x", "start_col": value, "end_col": value}})
            .to_string();
            let update = parse_update(&line).unwrap();
            assert_eq!(update.facts, PushFacts::default());
            assert_eq!(update.context.selection.unwrap().text, "x");
        }
    }

    #[test]
    fn the_context_forwarded_to_window_2_has_no_columns() {
        use std::io::Write;
        let mut feed = EditorContextFeed::new().expect("feed");
        let mut reader = EditorContextReader::new(feed.take_listener().unwrap());
        let line = serde_json::json!({"v": 1, "file": "/p/a.rs", "line": 3,
            "selection": {"start_line": 2, "end_line": 3, "text": "xy", "start_col": 1, "end_col": 4}});
        let mut client = std::os::unix::net::UnixStream::connect(feed.socket_path()).unwrap();
        writeln!(client, "{line}").unwrap();
        drop(client);
        let update = reader.poll_update().expect("the update arrives");
        assert_eq!(
            update.facts,
            PushFacts {
                cursor_line: Some(3),
                start_col: Some(1),
                end_col: Some(4)
            }
        );
        assert_eq!(
            serde_json::to_value(update.context).unwrap(),
            serde_json::json!({
                "file": "/p/a.rs", "selection": {"start_line": 2, "end_line": 3, "text": "xy"}
            })
        );
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

    /// An update naming a file whose name is not UTF-8 is still news: the file it names replaces the
    /// one before, its odd bytes shown as U+FFFD, rather than the update being dropped and the next turn
    /// naming the file that was open before it.
    #[test]
    fn an_update_with_bytes_that_are_not_utf8_still_names_its_file() {
        let context = parse_bytes(b"{\"v\":1,\"file\":\"/p/caf\xe9.rs\"}")
            .expect("the update is read")
            .context;
        assert_eq!(context.file, "/p/caf\u{fffd}.rs");
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
        for needle in ["EITRI_EDITOR_SOCKET", "getregion", "getpos"] {
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
    /// gives: an inequality lets the whole margin be eaten in silence. Every feed socket in this
    /// crate sits at exactly 100 against a 103-byte cap, so the remaining
    /// budget is three bytes for every future protocol combined -- which is why this wire's
    /// directory prefix and file name are both six bytes rather than something legible.
    #[test]
    fn the_editor_context_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        // macOS's measured temporary-directory shape: `/var/folders/<2>/<28>/T/` (49 bytes).
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
