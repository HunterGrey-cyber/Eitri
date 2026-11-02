//! The two editor reads behind `@buffers` and `@quickfix`. They are not tools: nothing declares them to a model, and
//! the host asks for them when a person's message names one. A chunk is a constant like a tool's, with the same
//! stamp, and its answer is judged in Rust before a line of it is told (`crate::mentions`).

use std::path::{Path, PathBuf};

use rmpv::Value;

use super::diagnostics::{count, field, list, name_of, text_of};
use super::path_value;
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::stamp::Stamp;
use crate::nvim_rpc::Pending;

pub const BUFFERS_LUA: &str = chunk!("mention_buffers.lua");
pub const QUICKFIX_LUA: &str = chunk!("mention_quickfix.lua");

/// The most buffers the read names.
pub const BUFFERS_LISTED_MAX: u32 = 100;
/// The most lines of one buffer, and the most bytes of its text (a line's terminator counts).
pub const BUFFER_LINES_MAX: u32 = 1000;
pub const BUFFER_BYTES_MAX: u32 = 32 * 1024;
/// The most text of every buffer together.
pub const BUFFERS_TOTAL_MAX: u32 = 96 * 1024;
/// The most quickfix entries the editor is asked for; the host tells fewer.
pub const QUICKFIX_SCAN_MAX: u32 = 2000;

/// One open buffer, as the chunk read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferRead {
    /// The buffer's name.
    pub path: PathBuf,
    /// What its text was read from, when the editor keeps a record; a buffer not loaded answers its own name.
    pub read_from: Option<PathBuf>,
    pub modified: bool,
    pub loaded: bool,
    pub current: bool,
    pub shown: bool,
    /// The buffer's whole length in lines.
    pub lines: u32,
    pub filetype: String,
    /// The lines the read took, from the first.
    pub text: Vec<String>,
    /// Set when the text stops short of the buffer: the lines taken.
    pub cut: Option<u32>,
    /// Set when the text stopped at the bound on every buffer together, not at this buffer's own.
    pub budget: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuffersAnswer {
    pub buffers: Vec<BufferRead>,
    /// File buffers outside the root, counted and not read.
    pub other: u32,
    /// File buffers under the root past [`BUFFERS_LISTED_MAX`].
    pub left_out: u32,
}

/// One quickfix entry. `path` is the buffer it points into; an entry that points nowhere has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickfixItem {
    pub path: Option<PathBuf>,
    pub read_from: Option<PathBuf>,
    pub lnum: u32,
    pub end_lnum: u32,
    pub col: u32,
    pub kind: String,
    pub valid: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickfixAnswer {
    pub title: String,
    /// The list's whole length.
    pub size: u32,
    pub items: Vec<QuickfixItem>,
    /// Entries past [`QUICKFIX_SCAN_MAX`].
    pub left_out: u32,
}

pub fn send_buffers(rpc: &dyn EditorRpc, stamp: &Stamp, root: &Path) -> Pending {
    let number = |name: &str, value: u32| (Value::from(name), Value::from(value));
    let args = vec![
        (Value::from("root"), path_value(root)),
        number("listed_max", BUFFERS_LISTED_MAX),
        number("lines_max", BUFFER_LINES_MAX),
        number("bytes_max", BUFFER_BYTES_MAX),
        number("total_max", BUFFERS_TOTAL_MAX),
    ];
    rpc.exec_lua(BUFFERS_LUA, vec![stamp.to_value(), Value::Map(args)])
}

pub fn send_quickfix(rpc: &dyn EditorRpc, stamp: &Stamp) -> Pending {
    let args = vec![(Value::from("scan_max"), Value::from(QUICKFIX_SCAN_MAX))];
    rpc.exec_lua(QUICKFIX_LUA, vec![stamp.to_value(), Value::Map(args)])
}

fn flag(value: &Value, name: &str) -> Option<bool> {
    field(value, name)?.as_bool()
}

fn unreadable(what: &str) -> String {
    format!("the editor's answer about {what} could not be read")
}

fn expired(value: &Value) -> bool {
    field(value, "expired").is_some()
}

fn buffer_of(value: &Value) -> Option<BufferRead> {
    Some(BufferRead {
        path: name_of(field(value, "path")?)?,
        read_from: field(value, "read_from").and_then(name_of),
        modified: flag(value, "modified")?,
        loaded: flag(value, "loaded")?,
        current: flag(value, "current")?,
        shown: flag(value, "shown")?,
        lines: count(value, "lines")?,
        filetype: text_of(field(value, "filetype")?)?,
        text: list(value, "text")?.iter().map(text_of).collect::<Option<Vec<_>>>()?,
        cut: count(value, "cut"),
        budget: flag(value, "budget").unwrap_or(false),
    })
}

pub fn parse_buffers(value: &Value) -> Result<BuffersAnswer, String> {
    if expired(value) {
        return Err(crate::editor_tools::words::NOT_READ.to_owned());
    }
    let fail = || unreadable("its open buffers");
    Ok(BuffersAnswer {
        buffers: list(value, "buffers")
            .ok_or_else(fail)?
            .iter()
            .map(buffer_of)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(fail)?,
        other: count(value, "other").ok_or_else(fail)?,
        left_out: count(value, "left_out").ok_or_else(fail)?,
    })
}

fn item_of(value: &Value) -> Option<QuickfixItem> {
    Some(QuickfixItem {
        path: field(value, "path").and_then(name_of),
        read_from: field(value, "read_from").and_then(name_of),
        lnum: count(value, "lnum")?,
        end_lnum: count(value, "end_lnum")?,
        col: count(value, "col")?,
        kind: text_of(field(value, "kind")?)?,
        valid: flag(value, "valid")?,
        text: text_of(field(value, "text")?)?,
    })
}

pub fn parse_quickfix(value: &Value) -> Result<QuickfixAnswer, String> {
    if expired(value) {
        return Err(crate::editor_tools::words::NOT_READ.to_owned());
    }
    let fail = || unreadable("its quickfix list");
    Ok(QuickfixAnswer {
        title: text_of(field(value, "title").ok_or_else(fail)?).ok_or_else(fail)?,
        size: count(value, "size").ok_or_else(fail)?,
        items: list(value, "items")
            .ok_or_else(fail)?
            .iter()
            .map(item_of)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(fail)?,
        left_out: count(value, "left_out").ok_or_else(fail)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(fields: Vec<(&str, Value)>) -> Value {
        Value::Map(fields.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    fn lines(text: &[&str]) -> Value {
        Value::Array(text.iter().map(|line| Value::from(*line)).collect())
    }

    #[test]
    fn a_buffers_answer_reads_back_and_an_empty_table_may_be_either_kind() {
        let value = map(vec![
            (
                "buffers",
                Value::Array(vec![map(vec![
                    ("path", Value::Binary(b"/p/a.rs".to_vec())),
                    ("read_from", Value::Binary(b"/p/a.rs".to_vec())),
                    ("modified", Value::from(true)),
                    ("loaded", Value::from(true)),
                    ("current", Value::from(true)),
                    ("shown", Value::from(false)),
                    ("lines", Value::from(3)),
                    ("filetype", Value::from("rust")),
                    ("text", lines(&["fn a() {}", "", "x"])),
                    ("cut", Value::from(2)),
                ])]),
            ),
            ("other", Value::from(1)),
            ("left_out", Value::from(0)),
        ]);
        let answer = parse_buffers(&value).unwrap();
        assert_eq!(answer.buffers[0].path, PathBuf::from("/p/a.rs"));
        assert!(answer.buffers[0].modified && answer.buffers[0].current);
        assert_eq!(answer.buffers[0].text, ["fn a() {}", "", "x"]);
        assert_eq!(answer.buffers[0].cut, Some(2));
        assert!(!answer.buffers[0].budget);
        assert_eq!((answer.other, answer.left_out), (1, 0));

        let empty = map(vec![
            ("buffers", Value::Map(vec![])),
            ("other", Value::from(0)),
            ("left_out", Value::from(0)),
        ]);
        assert!(parse_buffers(&empty).unwrap().buffers.is_empty());
    }

    #[test]
    fn a_chunk_that_expired_or_answered_something_else_is_an_error() {
        let expired = map(vec![("expired", Value::from(true))]);
        assert!(parse_buffers(&expired).is_err());
        assert!(parse_quickfix(&expired).is_err());
        assert!(parse_buffers(&Value::from("no")).is_err());
        assert!(parse_quickfix(&map(vec![("title", Value::from("t"))])).is_err());
    }

    #[test]
    fn a_quickfix_answer_reads_back_with_and_without_a_file() {
        let value = map(vec![
            ("title", Value::from(":make")),
            ("size", Value::from(2)),
            (
                "items",
                Value::Array(vec![
                    map(vec![
                        ("path", Value::Binary(b"/p/a.rs".to_vec())),
                        ("read_from", Value::Binary(b"/p/a.rs".to_vec())),
                        ("lnum", Value::from(4)),
                        ("end_lnum", Value::from(0)),
                        ("col", Value::from(2)),
                        ("kind", Value::from("E")),
                        ("valid", Value::from(true)),
                        ("text", Value::from("mismatched types")),
                    ]),
                    map(vec![
                        ("lnum", Value::from(0)),
                        ("end_lnum", Value::from(0)),
                        ("col", Value::from(0)),
                        ("kind", Value::from("")),
                        ("valid", Value::from(false)),
                        ("text", Value::from("make: *** Error 1")),
                    ]),
                ]),
            ),
            ("left_out", Value::from(0)),
        ]);
        let answer = parse_quickfix(&value).unwrap();
        assert_eq!(answer.title, ":make");
        assert_eq!(answer.items[0].path.as_deref(), Some(Path::new("/p/a.rs")));
        assert_eq!(answer.items[1].path, None);
        assert!(!answer.items[1].valid);
    }

    #[test]
    fn both_chunks_open_with_the_stamp_check() {
        let prelude = include_str!("../prelude.lua");
        for chunk in [BUFFERS_LUA, QUICKFIX_LUA] {
            assert!(chunk.starts_with(prelude));
            mlua::Lua::new().load(chunk).into_function().expect("Lua source");
        }
    }
}
