//! The strict parse of a record and of the handoff nvim leaves at its exit. Whatever wrote the text, only what the
//! record may hold comes out: every key known, every count, size, number and name within its bound.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use base64::Engine as _;
use serde_json::{Map, Value};

use super::{
    Leaf, Node, Record, Tab, MAX_BUFS, MAX_CELLS, MAX_DEPTH, MAX_LEAVES, MAX_NAME, MAX_POSITION, MAX_TABS, RECORD_MAX,
    VERSION,
};

/// JSON nesting allowed before the text is parsed: a tree [`MAX_DEPTH`] deep takes two JSON levels a node, and the
/// record and the handoff wrap it in four more. Deeper text is refused before `serde_json` reads it, whatever
/// recursion limit that parser keeps.
const MAX_NESTING: usize = 40;

/// Why a record was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// Over [`RECORD_MAX`] bytes.
    TooLarge,
    /// Nested past what any record holds.
    TooDeep,
    /// Not JSON, or not the record's shape.
    Syntax(String),
    /// Another format version: read as nothing, and replaced by the next write.
    Version(u64),
    /// A record for another project root: read as nothing.
    OtherRoot,
    /// A count, size, number or name past its bound; names which.
    Bound(&'static str),
}

impl RecordError {
    /// Whether the record is simply not this window's to use -- another version, another root -- rather than
    /// broken: such a file is left where it is, not set aside.
    pub fn reads_as_nothing(&self) -> bool {
        matches!(self, RecordError::Version(_) | RecordError::OtherRoot)
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::TooLarge => write!(f, "over {RECORD_MAX} bytes"),
            RecordError::TooDeep => write!(f, "nested deeper than {MAX_NESTING} levels"),
            RecordError::Syntax(why) => write!(f, "not a record: {why}"),
            RecordError::Version(v) => write!(f, "version {v}, not {VERSION}"),
            RecordError::OtherRoot => write!(f, "a record of another project root"),
            RecordError::Bound(what) => write!(f, "{what} out of bounds"),
        }
    }
}

impl std::error::Error for RecordError {}

/// Reads a record for `root`. A record of another version or another root is an error that
/// [`RecordError::reads_as_nothing`].
pub fn parse(json: &str, root: &Path) -> Result<Record, RecordError> {
    record(&value(json)?, root)
}

/// Reads the handoff nvim leaves at its exit: `{"same": <bool>, "record": <record>}` and nothing else. `same` says
/// the record is the one the restore itself produced.
pub fn parse_handoff(json: &str, root: &Path) -> Result<(Record, bool), RecordError> {
    let value = value(json)?;
    let object = object(&value, "the handoff")?;
    only_keys(object, &["same", "record"], "the handoff")?;
    let same = object
        .get("same")
        .and_then(Value::as_bool)
        .ok_or_else(|| syntax("the handoff's same is not true or false"))?;
    let record = record(
        object
            .get("record")
            .ok_or_else(|| syntax("the handoff has no record"))?,
        root,
    )?;
    Ok((record, same))
}

/// The size bound, then the nesting bound, then the JSON parse.
fn value(json: &str) -> Result<Value, RecordError> {
    if json.len() > RECORD_MAX {
        return Err(RecordError::TooLarge);
    }
    if !nesting_within(json, MAX_NESTING) {
        return Err(RecordError::TooDeep);
    }
    serde_json::from_str(json).map_err(|e| RecordError::Syntax(e.to_string()))
}

/// Whether no `[` or `{` outside a JSON string is nested more than `max` deep. A string runs from an unescaped `"`
/// to the next unescaped `"`; a backslash escapes the byte after it, so `\"` stays in the string and `\\` does not
/// escape what follows. Malformed text gets an answer too; the parse after this judges it.
fn nesting_within(text: &str, max: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &b in text.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > max {
                    return false;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

fn syntax(why: impl Into<String>) -> RecordError {
    RecordError::Syntax(why.into())
}

/// The version first, then the root, so a record of another version or root reads as nothing whatever else it
/// holds; then every other key.
fn record(value: &Value, root: &Path) -> Result<Record, RecordError> {
    let object = object(value, "the record")?;
    let version = object
        .get("v")
        .ok_or_else(|| syntax("the record has no version"))?
        .as_u64()
        .ok_or_else(|| syntax("the record's version is not a whole number"))?;
    if version != VERSION {
        return Err(RecordError::Version(version));
    }
    let root_bytes = decode(
        object.get("root").ok_or_else(|| syntax("the record has no root"))?,
        "root",
    )?;
    if root_bytes != root.as_os_str().as_bytes() {
        return Err(RecordError::OtherRoot);
    }
    only_keys(object, &["v", "root", "tab", "tabs", "bufs"], "the record")?;

    let tab_values = list(object.get("tabs"), "tabs")?;
    if tab_values.len() > MAX_TABS {
        return Err(RecordError::Bound("tabs"));
    }
    let tabs = tab_values.iter().map(tab).collect::<Result<Vec<_>, _>>()?;

    let tab = match object.get("tab") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| syntax("tab is not a whole number"))?;
            if n == 0 || n > tabs.len() as u64 {
                return Err(RecordError::Bound("tab"));
            }
            Some(n as u32)
        }
    };

    let buf_values = list(object.get("bufs"), "bufs")?;
    if buf_values.len() > MAX_BUFS {
        return Err(RecordError::Bound("bufs"));
    }
    let bufs = buf_values.iter().map(name).collect::<Result<Vec<_>, _>>()?;

    Ok(Record {
        root: root_bytes,
        tab,
        tabs,
        bufs,
    })
}

fn tab(value: &Value) -> Result<Tab, RecordError> {
    let object = object(value, "a tab")?;
    only_keys(object, &["cols", "lines", "tree"], "a tab")?;
    let cols = number(object, "cols", 1, MAX_CELLS)?;
    let lines = number(object, "lines", 1, MAX_CELLS)?;
    let mut count = Count::default();
    let tree = node(
        object.get("tree").ok_or_else(|| syntax("a tab has no tree"))?,
        1,
        &mut count,
    )?;
    Ok(Tab { cols, lines, tree })
}

/// The windows and current windows seen so far in one tab page.
#[derive(Default)]
struct Count {
    leaves: usize,
    current: usize,
}

fn node(value: &Value, depth: usize, count: &mut Count) -> Result<Node, RecordError> {
    if depth > MAX_DEPTH {
        return Err(RecordError::Bound("depth"));
    }
    let object = object(value, "a window tree")?;
    let mut entries = object.iter();
    let (Some((kind, inner)), None) = (entries.next(), entries.next()) else {
        return Err(syntax("a window tree node is not one of leaf, row or col"));
    };
    match kind.as_str() {
        "leaf" => {
            count.leaves += 1;
            if count.leaves > MAX_LEAVES {
                return Err(RecordError::Bound("leaves in a tab"));
            }
            let leaf = leaf(inner)?;
            if leaf.cur {
                count.current += 1;
                if count.current > 1 {
                    return Err(RecordError::Bound("cur"));
                }
            }
            Ok(Node::Leaf(leaf))
        }
        "row" | "col" => {
            let children = list(Some(inner), "a row or col")?
                .iter()
                .map(|child| node(child, depth + 1, count))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(if kind == "row" {
                Node::Row(children)
            } else {
                Node::Col(children)
            })
        }
        other => Err(syntax(format!("a window tree node is {other:?}, not leaf, row or col"))),
    }
}

fn leaf(value: &Value) -> Result<Leaf, RecordError> {
    let object = object(value, "a window")?;
    only_keys(
        object,
        &["f", "lnum", "col", "want", "top", "left", "w", "h", "cur"],
        "a window",
    )?;
    Ok(Leaf {
        file: name(object.get("f").ok_or_else(|| syntax("a window has no file"))?)?,
        lnum: number(object, "lnum", 1, MAX_POSITION)?,
        col: number(object, "col", 0, MAX_POSITION)?,
        want: number(object, "want", 0, MAX_POSITION)?,
        top: number(object, "top", 1, MAX_POSITION)?,
        left: number(object, "left", 0, MAX_POSITION)?,
        w: number(object, "w", 1, MAX_CELLS)?,
        h: number(object, "h", 1, MAX_CELLS)?,
        cur: object
            .get("cur")
            .and_then(Value::as_bool)
            .ok_or_else(|| syntax("a window's cur is not true or false"))?,
    })
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, RecordError> {
    value
        .as_object()
        .ok_or_else(|| syntax(format!("{what} is not an object")))
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), RecordError> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(syntax(format!("{what} holds an unknown key {key:?}"))),
        None => Ok(()),
    }
}

/// A list: an array, or an empty object, which is what some nvim builds encode an empty Lua table as.
fn list<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a [Value], RecordError> {
    match value {
        Some(Value::Array(items)) => Ok(items),
        Some(Value::Object(map)) if map.is_empty() => Ok(&[]),
        Some(_) => Err(syntax(format!("{what} is not a list"))),
        None => Err(syntax(format!("{what} is missing"))),
    }
}

/// A whole number in `lo..=hi`, judged as the JSON gave it before it is narrowed.
fn number(object: &Map<String, Value>, key: &'static str, lo: u32, hi: u32) -> Result<u32, RecordError> {
    let n = object
        .get(key)
        .ok_or_else(|| syntax(format!("{key} is missing")))?
        .as_u64()
        .ok_or_else(|| syntax(format!("{key} is not a whole number")))?;
    if n < u64::from(lo) || n > u64::from(hi) {
        return Err(RecordError::Bound(key));
    }
    Ok(n as u32)
}

fn decode(value: &Value, what: &str) -> Result<Vec<u8>, RecordError> {
    let text = value
        .as_str()
        .ok_or_else(|| syntax(format!("{what} is not a string")))?;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| syntax(format!("{what} is not base64: {e}")))
}

/// An absolute file name of 1 to [`MAX_NAME`] bytes with no NUL.
fn name(value: &Value) -> Result<Vec<u8>, RecordError> {
    let bytes = decode(value, "a file name")?;
    if bytes.is_empty() || bytes.len() > MAX_NAME {
        return Err(RecordError::Bound("name length"));
    }
    if bytes.contains(&0) {
        return Err(RecordError::Bound("NUL in name"));
    }
    if bytes[0] != b'/' {
        return Err(RecordError::Bound("relative name"));
    }
    Ok(bytes)
}
