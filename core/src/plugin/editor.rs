//! The fixed Lua chunks a plugin's host runs in the user's editor, and how their answers are read. The
//! Lua is a compile-time constant and every value -- a path, a line, a setting -- travels as an argument,
//! so nothing a page sends can become Lua or Ex source in the user's nvim.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rmpv::Value;

use crate::editor_rpc::{EditorRpc, GlueSlot};
use crate::nvim_rpc::Pending;

use super::protocol::JumpSelect;

// Statics, not consts: a const is copied to each use, and the tests check that the very same text is what
// reaches the editor, by address.
pub static PLUGIN_BUFFERS_LUA: &str = include_str!("plugin_buffers.lua");
pub static PLUGIN_CWD_LUA: &str = include_str!("plugin_cwd.lua");
pub static PLUGIN_OPEN_LUA: &str = include_str!("plugin_open.lua");
pub static PLUGIN_ASSIST_LUA: &str = include_str!("plugin_assist.lua");
pub static PLUGIN_FOLLOW_LUA: &str = include_str!("plugin_follow.lua");

/// A path as bytes: nvim gives Lua a string either way, and bytes are never re-encoded.
fn path_value(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

pub fn open(rpc: &dyn EditorRpc, path: &Path, line: Option<u32>, col: Option<u32>) -> Pending {
    open_owned(rpc, "", path, line, col)
}

pub fn open_owned(rpc: &dyn EditorRpc, owner: &str, path: &Path, line: Option<u32>, col: Option<u32>) -> Pending {
    let mut opts = vec![
        (Value::from("path"), path_value(path)),
        (Value::from("owner"), Value::from(owner)),
    ];
    if let Some(line) = line {
        opts.push((Value::from("line"), Value::from(line)));
    }
    if let Some(col) = col {
        opts.push((Value::from("col"), Value::from(col)));
    }
    rpc.exec_lua(PLUGIN_OPEN_LUA, vec![Value::Map(opts)])
}

/// Where an assist call stands in the order this process made them: `maker` names the process, `order` counts up.
///
/// The embedded editor can, rarely, write a call ahead of one made just before it (its call line polls the
/// requests in order, but nvim-rs takes two locks of its own before it writes, and another writer can free one in
/// the instant between two polls). Two assist calls swapped would leave the wrong one in force for good: an apply
/// landing after the clear that should undo it keeps its settings after the plugin stopped. So each assist call
/// says when it was made, and the snippet skips one older than a call it has already run for the same owner and
/// maker. One counter for the whole process, so a plugin stopped and started again keeps counting up; the maker
/// tells this process from another attached to the same nvim, whose count is its own.
fn assist_order() -> [(Value, Value); 2] {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static MAKER: OnceLock<String> = OnceLock::new();
    let maker = MAKER.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{nanos}", std::process::id())
    });
    [
        (Value::from("maker"), Value::from(maker.as_str())),
        (Value::from("order"), Value::from(NEXT.fetch_add(1, Ordering::Relaxed))),
    ]
}

pub fn assist_apply(rpc: &dyn EditorRpc, owner: &str, root: &Path, diagnostics: bool, inlay_hints: bool) -> Pending {
    let mut opts = vec![
        (Value::from("owner"), Value::from(owner)),
        (Value::from("root"), path_value(root)),
        (Value::from("diagnostics"), Value::from(diagnostics)),
        (Value::from("inlay_hints"), Value::from(inlay_hints)),
    ];
    opts.extend(assist_order());
    rpc.exec_lua(PLUGIN_ASSIST_LUA, vec![Value::from("apply"), Value::Map(opts)])
}

pub fn assist_clear(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    let mut opts = vec![(Value::from("owner"), Value::from(owner))];
    opts.extend(assist_order());
    rpc.exec_lua(PLUGIN_ASSIST_LUA, vec![Value::from("clear"), Value::Map(opts)])
}

/// Cwd calls carry the same process/order stamp as assists, so a delayed cd cannot overtake its clear.
pub fn cwd(rpc: &dyn EditorRpc, owner: &str, dir: &Path) -> Pending {
    let mut opts = vec![
        (Value::from("owner"), Value::from(owner)),
        (Value::from("dir"), path_value(dir)),
    ];
    opts.extend(assist_order());
    rpc.exec_lua(PLUGIN_CWD_LUA, vec![Value::from("cd"), Value::Map(opts)])
}

pub fn cwd_clear(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    let mut opts = vec![(Value::from("owner"), Value::from(owner))];
    opts.extend(assist_order());
    rpc.exec_lua(PLUGIN_CWD_LUA, vec![Value::from("clear"), Value::Map(opts)])
}

pub fn preload(rpc: &dyn EditorRpc, owner: &str, path: &Path, diagnostics: bool, inlay_hints: bool) -> Pending {
    buffers_call(
        rpc,
        owner,
        "preload",
        vec![
            (Value::from("path"), path_value(path)),
            (Value::from("diagnostics"), Value::from(diagnostics)),
            (Value::from("inlay_hints"), Value::from(inlay_hints)),
        ],
    )
}

pub fn modified(rpc: &dyn EditorRpc, owner: &str, root: &Path) -> Pending {
    buffers_call(rpc, owner, "modified", vec![(Value::from("root"), path_value(root))])
}

pub fn reload(rpc: &dyn EditorRpc, owner: &str, root: &Path) -> Pending {
    buffers_call(rpc, owner, "reload", vec![(Value::from("root"), path_value(root))])
}

pub fn wipe_preload(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    buffers_call(rpc, owner, "wipe_preload", vec![])
}

pub fn diff(rpc: &dyn EditorRpc, owner: &str, args: &super::protocol::EditorDiff) -> Pending {
    let side = |side: &super::protocol::DiffSide| match side {
        super::protocol::DiffSide::Path { path } => Value::Map(vec![("path".into(), path_value(path))]),
        super::protocol::DiffSide::Text { title, text, filetype } => {
            let mut fields = vec![
                ("title".into(), title.as_str().into()),
                ("text".into(), text.as_str().into()),
            ];
            // An absent key reaches Lua as nil; a msgpack nil inside a map would arrive as vim.NIL instead.
            if let Some(filetype) = filetype {
                fields.push(("filetype".into(), filetype.as_str().into()));
            }
            Value::Map(fields)
        }
    };
    buffers_call(
        rpc,
        owner,
        "diff",
        vec![("left".into(), side(&args.left)), ("right".into(), side(&args.right))],
    )
}

pub fn close_diffs(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    buffers_call(rpc, owner, "close_diffs", vec![])
}

fn buffers_call(rpc: &dyn EditorRpc, owner: &str, op: &str, mut opts: Vec<(Value, Value)>) -> Pending {
    opts.push((Value::from("owner"), Value::from(owner)));
    opts.extend(assist_order());
    rpc.exec_lua(PLUGIN_BUFFERS_LUA, vec![Value::from(op), Value::Map(opts)])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preloaded {
    pub buf: u64,
    pub replaced: bool,
    pub stopped_clients: u64,
}

pub fn decode_preload(answer: &Value) -> Result<Preloaded, String> {
    Ok(Preloaded {
        buf: count(answer, "buf")?,
        replaced: flag(answer, "replaced")?,
        stopped_clients: count(answer, "stopped_clients")?,
    })
}

pub fn decode_modified(answer: &Value) -> Result<Vec<String>, String> {
    list(answer, "paths")?
        .iter()
        .map(|v| lossy_text(v).ok_or_else(|| "the editor's answer has a path that is not text".to_string()))
        .collect()
}

pub fn decode_reload(answer: &Value) -> Result<(u64, u64), String> {
    Ok((count(answer, "reloaded")?, count(answer, "kept_modified")?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedCwd {
    pub dir: String,
    pub tab: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearedCwd {
    pub restored: u64,
    pub moved: u64,
}

pub fn decode_cwd(answer: &Value) -> Result<ChangedCwd, String> {
    Ok(ChangedCwd {
        dir: field(answer, "dir")
            .and_then(lossy_text)
            .ok_or_else(|| "the editor's answer has no dir".to_string())?,
        tab: count(answer, "tab")?,
    })
}

pub fn decode_cwd_clear(answer: &Value) -> Result<ClearedCwd, String> {
    Ok(ClearedCwd {
        restored: count(answer, "restored")?,
        moved: count(answer, "moved")?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistApplied {
    pub diagnostics: bool,
    pub inlay_hints: bool,
    pub buffers: u64,
    pub partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistCleared {
    pub restored: u64,
    pub left_alone: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub how: String,
    pub line: u64,
    pub col: u64,
}

fn field<'a>(answer: &'a Value, key: &str) -> Option<&'a Value> {
    answer
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn flag(answer: &Value, key: &str) -> Result<bool, String> {
    field(answer, key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("the editor's answer has no {key}"))
}

fn count(answer: &Value, key: &str) -> Result<u64, String> {
    field(answer, key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("the editor's answer has no {key}"))
}

pub fn decode_apply(answer: &Value) -> Result<AssistApplied, String> {
    Ok(AssistApplied {
        diagnostics: flag(answer, "diagnostics")?,
        inlay_hints: flag(answer, "inlay_hints")?,
        buffers: count(answer, "buffers")?,
        partial: flag(answer, "partial")?,
    })
}

pub fn decode_clear(answer: &Value) -> Result<AssistCleared, String> {
    Ok(AssistCleared {
        restored: count(answer, "restored")?,
        left_alone: count(answer, "left_alone")?,
    })
}

pub fn decode_open(answer: &Value) -> Result<Opened, String> {
    let how = field(answer, "how")
        .and_then(Value::as_str)
        .ok_or_else(|| "the editor's answer has no how".to_string())?
        .to_owned();
    Ok(Opened {
        how,
        line: count(answer, "line")?,
        col: count(answer, "col")?,
    })
}

/// What `install` gives the follow snippet. `commands` are `(name, does)`, `does` one of `show`, `toggle`, `hide`.
pub struct FollowInstall<'a> {
    pub owner: &'a str,
    pub socket: &'a Path,
    pub filetypes: &'a [String],
    pub auto: bool,
    pub commands: &'a [(String, &'static str)],
    /// Eitri's own scratch buffers live here; none is ever followed.
    pub scratch_dir: Option<&'a Path>,
}

fn follow_call(rpc: &dyn EditorRpc, op: &str, mut opts: Vec<(Value, Value)>) -> Pending {
    if let Some(glue) = rpc.glue() {
        let mut slot = vec![(Value::from("chan"), Value::from(glue.chan))];
        // An absent id names the agent slot; page ids always name entries in the page registry.
        if let GlueSlot::Page(id) = glue.slot {
            slot.push((Value::from("id"), Value::from(id)));
        }
        opts.push((Value::from("slot"), Value::Map(slot)));
    }
    rpc.exec_lua(PLUGIN_FOLLOW_LUA, vec![Value::from(op), Value::Map(opts)])
}

pub fn follow_install(rpc: &dyn EditorRpc, install: &FollowInstall<'_>) -> Pending {
    follow_install_with_scratch_prefix(rpc, install, None)
}

pub(super) fn follow_install_with_scratch_prefix(
    rpc: &dyn EditorRpc,
    install: &FollowInstall<'_>,
    scratch_prefix: Option<&Path>,
) -> Pending {
    let filetypes = install.filetypes.iter().map(|f| Value::from(f.as_str())).collect();
    let commands = install
        .commands
        .iter()
        .map(|(name, does)| Value::Array(vec![Value::from(name.as_str()), Value::from(*does)]))
        .collect();
    let mut opts = vec![
        (Value::from("owner"), Value::from(install.owner)),
        (Value::from("socket"), path_value(install.socket)),
        (Value::from("filetypes"), Value::Array(filetypes)),
        (Value::from("auto"), Value::from(install.auto)),
        (Value::from("commands"), Value::Array(commands)),
    ];
    if let Some(dir) = install.scratch_dir {
        opts.push((Value::from("scratch_dir"), path_value(dir)));
    }
    if let Some(prefix) = scratch_prefix {
        opts.push((Value::from("scratch_prefix"), path_value(prefix)));
    }
    follow_call(rpc, "install", opts)
}

pub fn follow_on(rpc: &dyn EditorRpc, owner: &str, on: bool) -> Pending {
    follow_call(
        rpc,
        "follow",
        vec![
            (Value::from("owner"), Value::from(owner)),
            (Value::from("on"), Value::from(on)),
        ],
    )
}

pub fn follow_text(rpc: &dyn EditorRpc, owner: &str, buf: u64, max_bytes: u64, max_lines: u64) -> Pending {
    follow_call(
        rpc,
        "text",
        vec![
            (Value::from("owner"), Value::from(owner)),
            (Value::from("buf"), Value::from(buf)),
            (Value::from("max_bytes"), Value::from(max_bytes)),
            (Value::from("max_lines"), Value::from(max_lines)),
        ],
    )
}

/// `cursor`, when given, is the line the window's cursor goes to. With `row`, its line starts on that screen row
/// of nvim's whole grid where possible; otherwise it is held inside the window as it then stands.
pub fn follow_reveal(
    rpc: &dyn EditorRpc,
    owner: &str,
    buf: u64,
    line: u32,
    token: u64,
    cursor: Option<u32>,
    row: Option<u32>,
) -> Pending {
    let mut args = vec![
        (Value::from("owner"), Value::from(owner)),
        (Value::from("buf"), Value::from(buf)),
        (Value::from("line"), Value::from(line)),
        (Value::from("token"), Value::from(token)),
    ];
    if let Some(cursor) = cursor {
        args.push((Value::from("cursor"), Value::from(cursor)));
    }
    if let Some(row) = row {
        args.push((Value::from("row"), Value::from(row)));
    }
    follow_call(rpc, "reveal", args)
}

/// `col` counts Unicode scalar values from 1; 0 asks for the line's first non-blank character. With `select`, nvim
/// selects from (`line`, `col`) to that place in its visual mode, its cursor ending there.
pub fn follow_jump(
    rpc: &dyn EditorRpc,
    owner: &str,
    buf: u64,
    line: u32,
    col: u32,
    select: Option<JumpSelect>,
) -> Pending {
    let mut args = vec![
        (Value::from("owner"), Value::from(owner)),
        (Value::from("buf"), Value::from(buf)),
        (Value::from("line"), Value::from(line)),
        (Value::from("col"), Value::from(col)),
    ];
    if let Some(select) = select {
        args.push((Value::from("select_line"), Value::from(select.line)));
        args.push((Value::from("select_col"), Value::from(select.col)));
        args.push((Value::from("select_mode"), Value::from(select.mode.as_str())));
    }
    follow_call(rpc, "jump", args)
}

pub fn follow_teardown(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    follow_call(rpc, "teardown", vec![(Value::from("owner"), Value::from(owner))])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Command names the user already had, in the order asked; none of them was replaced.
    pub skipped: Vec<String>,
    /// nvim had not finished starting, so nothing was done: the user's config may still define its own commands,
    /// and the install has to be asked again.
    pub early: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulledText {
    /// Invalid UTF-8 in a line is replaced with U+FFFD.
    Lines { buf: u64, tick: u64, lines: Vec<String> },
    TooLarge {
        buf: u64,
        tick: u64,
        bytes: u64,
        lines: u64,
    },
    /// The buffer is gone, unloaded, or no longer one the snippet follows.
    Gone { buf: u64 },
}

/// A Lua list as nvim sends it: an empty table may come back as an empty map rather than an empty array.
fn list<'a>(answer: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match field(answer, key) {
        Some(Value::Array(items)) => Ok(items),
        Some(Value::Map(entries)) if entries.is_empty() => Ok(&[]),
        _ => Err(format!("the editor's answer has no {key}")),
    }
}

/// A line's text whether nvim sent it as a msgpack string (which may hold invalid UTF-8) or as binary.
fn lossy_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
        Value::Binary(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

fn is_true(answer: &Value, key: &str) -> bool {
    field(answer, key).and_then(Value::as_bool) == Some(true)
}

pub fn read_installed(answer: &Value) -> Result<Installed, String> {
    if !flag(answer, "ok")? {
        if is_true(answer, "early") {
            return Ok(Installed {
                skipped: Vec::new(),
                early: true,
            });
        }
        return Err("the editor did not install the follow snippet".to_string());
    }
    let skipped = list(answer, "skipped")?
        .iter()
        .map(|v| lossy_text(v).ok_or_else(|| "the editor's answer has a skipped name that is not text".to_string()))
        .collect::<Result<_, _>>()?;
    Ok(Installed { skipped, early: false })
}

pub fn read_text(answer: &Value) -> Result<PulledText, String> {
    let buf = count(answer, "buf")?;
    if is_true(answer, "gone") {
        return Ok(PulledText::Gone { buf });
    }
    let tick = count(answer, "tick")?;
    if is_true(answer, "too_large") {
        return Ok(PulledText::TooLarge {
            buf,
            tick,
            bytes: count(answer, "bytes")?,
            lines: count(answer, "lines")?,
        });
    }
    let lines = list(answer, "lines")?
        .iter()
        .map(|v| lossy_text(v).ok_or_else(|| "the editor's answer has a line that is not text".to_string()))
        .collect::<Result<_, _>>()?;
    Ok(PulledText::Lines { buf, tick, lines })
}

pub fn read_applied(answer: &Value) -> Result<bool, String> {
    flag(answer, "applied")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim_rpc::RpcError;
    use std::cell::RefCell;

    /// Records what it was asked and answers that there is no editor.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Recorder {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::failed(RpcError::Unavailable("test".into()))
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    #[test]
    fn every_value_is_an_argument_of_a_constant_chunk() {
        let rpc = Recorder::default();
        let _ = open(&rpc, Path::new("/w/ex/src/lib.rs"), Some(3), Some(5));
        let _ = assist_apply(&rpc, "demo", Path::new("/w/ex"), false, true);
        let _ = assist_clear(&rpc, "demo");
        let calls = rpc.0.borrow();
        assert!(std::ptr::eq(calls[0].0, PLUGIN_OPEN_LUA));
        assert!(std::ptr::eq(calls[1].0, PLUGIN_ASSIST_LUA) && std::ptr::eq(calls[2].0, PLUGIN_ASSIST_LUA));
        let path = calls[0].1[0]
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some("path"))
            .unwrap()
            .1
            .clone();
        assert_eq!(
            path,
            Value::Binary(b"/w/ex/src/lib.rs".to_vec()),
            "the path is bytes, never source"
        );
        assert_eq!(calls[1].1[0], Value::from("apply"));
        assert_eq!(calls[2].1[0], Value::from("clear"));
    }

    #[test]
    fn buffer_calls_use_constant_lua_arguments_and_decode_the_wire_shapes() {
        let rpc = Recorder::default();
        let path = Path::new("/w/work/demo/src/lib.rs");
        let _ = preload(&rpc, "demo", path, true, false);
        let _ = modified(&rpc, "demo", path.parent().unwrap());
        let _ = reload(&rpc, "demo", path.parent().unwrap());
        let _ = wipe_preload(&rpc, "demo");
        let calls = rpc.0.borrow();
        for (call, op) in calls.iter().zip(["preload", "modified", "reload", "wipe_preload"]) {
            assert!(std::ptr::eq(call.0, PLUGIN_BUFFERS_LUA));
            assert_eq!(call.1[0], Value::from(op));
            assert_eq!(field(&call.1[1], "owner"), Some(&Value::from("demo")));
        }
        assert_eq!(field(&calls[0].1[1], "path"), Some(&path_value(path)));
        assert_eq!(field(&calls[0].1[1], "diagnostics"), Some(&Value::from(true)));
        assert_eq!(field(&calls[0].1[1], "inlay_hints"), Some(&Value::from(false)));
        assert_eq!(field(&calls[0].1[1], "maker"), field(&calls[3].1[1], "maker"));
        assert!(field(&calls[0].1[1], "order").unwrap().as_u64() < field(&calls[3].1[1], "order").unwrap().as_u64());
        let answer = Value::Map(vec![
            ("buf".into(), 7.into()),
            ("replaced".into(), true.into()),
            ("stopped_clients".into(), 1.into()),
        ]);
        assert_eq!(
            decode_preload(&answer),
            Ok(Preloaded {
                buf: 7,
                replaced: true,
                stopped_clients: 1
            })
        );
        assert_eq!(
            decode_modified(&Value::Map(vec![("paths".into(), Value::Map(vec![]))])),
            Ok(vec![])
        );
        assert!(decode_preload(&Value::Nil).is_err());
        assert!(decode_modified(&Value::Map(vec![("paths".into(), Value::Array(vec![false.into()]))])).is_err());
        assert!(decode_reload(&Value::Nil).is_err());
    }

    /// The snippet keeps the call made last in force even when two reach nvim swapped, so each one has to say
    /// when it was made: by the same maker, counting up.
    #[test]
    fn assist_calls_carry_the_order_they_were_made_in() {
        let rpc = Recorder::default();
        let _ = assist_apply(&rpc, "demo", Path::new("/w/ex"), false, true);
        let _ = assist_clear(&rpc, "demo");
        let _ = assist_apply(&rpc, "other", Path::new("/w/ex"), true, true);
        let calls = rpc.0.borrow();
        let made: Vec<(String, u64)> = calls
            .iter()
            .map(|(_, args)| {
                let maker = field(&args[1], "maker")
                    .and_then(Value::as_str)
                    .expect("a maker")
                    .to_owned();
                let order = field(&args[1], "order").and_then(Value::as_u64).expect("an order");
                (maker, order)
            })
            .collect();
        assert!(!made[0].0.is_empty());
        assert!(
            made.iter().all(|(maker, _)| *maker == made[0].0),
            "one maker per process: {made:?}"
        );
        assert!(
            made[0].1 < made[1].1 && made[1].1 < made[2].1,
            "counting up across owners: {made:?}"
        );
    }

    #[test]
    fn answers_decode_and_a_malformed_one_says_what_is_missing() {
        let m = |pairs: &[(&str, Value)]| Value::Map(pairs.iter().map(|(k, v)| (Value::from(*k), v.clone())).collect());
        let applied = m(&[
            ("diagnostics", false.into()),
            ("inlay_hints", true.into()),
            ("buffers", 2.into()),
            ("partial", false.into()),
        ]);
        assert_eq!(
            decode_apply(&applied),
            Ok(AssistApplied {
                diagnostics: false,
                inlay_hints: true,
                buffers: 2,
                partial: false
            })
        );
        assert_eq!(
            decode_clear(&m(&[("restored", 3.into()), ("left_alone", 1.into())])),
            Ok(AssistCleared {
                restored: 3,
                left_alone: 1
            })
        );
        assert_eq!(
            decode_open(&m(&[("how", "split".into()), ("line", 4.into()), ("col", 0.into())]))
                .unwrap()
                .how,
            "split"
        );
        assert!(decode_apply(&m(&[("buffers", 1.into())]))
            .unwrap_err()
            .contains("diagnostics"));
    }

    fn get<'a>(map: &'a Value, key: &str) -> &'a Value {
        field(map, key).unwrap_or_else(|| panic!("no {key} in {map:?}"))
    }

    #[test]
    fn every_follow_value_is_an_argument_of_the_follow_chunk() {
        let rpc = Recorder::default();
        let filetypes = vec!["markdown".to_string()];
        let commands = vec![("EitriMarkdown".to_string(), "show")];
        let mut install = FollowInstall {
            owner: "demo",
            socket: Path::new("/run/x/follow"),
            filetypes: &filetypes,
            auto: true,
            commands: &commands,
            scratch_dir: Some(Path::new("/tmp/s")),
        };
        let _ = follow_install(&rpc, &install);
        let _ = follow_on(&rpc, "demo", false);
        let _ = follow_text(&rpc, "demo", 3, 4_194_304, 50_000);
        let _ = follow_reveal(&rpc, "demo", 3, 50, 7, None, None);
        let _ = follow_jump(&rpc, "demo", 3, 5, 0, None);
        let _ = follow_teardown(&rpc, "demo");
        install.scratch_dir = None;
        let _ = follow_install(&rpc, &install);
        let _ = follow_reveal(&rpc, "demo", 3, 50, 8, Some(54), Some(9));
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 8);
        let ops = [
            "install", "follow", "text", "reveal", "jump", "teardown", "install", "reveal",
        ];
        for (call, op) in calls.iter().zip(ops) {
            assert!(
                std::ptr::eq(call.0, PLUGIN_FOLLOW_LUA),
                "{op} runs the follow chunk itself"
            );
            assert_eq!(call.1.len(), 2, "{op} takes (op, opts)");
            assert_eq!(call.1[0], Value::from(op));
            assert_eq!(get(&call.1[1], "owner"), &Value::from("demo"), "{op}");
        }
        let opts = &calls[0].1[1];
        assert_eq!(
            get(opts, "socket"),
            &Value::Binary(b"/run/x/follow".to_vec()),
            "a path is bytes"
        );
        assert_eq!(get(opts, "scratch_dir"), &Value::Binary(b"/tmp/s".to_vec()));
        assert_eq!(get(opts, "filetypes"), &Value::Array(vec![Value::from("markdown")]));
        assert_eq!(
            get(opts, "commands"),
            &Value::Array(vec![Value::Array(vec![
                Value::from("EitriMarkdown"),
                Value::from("show")
            ])])
        );
        assert_eq!(get(opts, "auto"), &Value::from(true));
        assert_eq!(get(&calls[1].1[1], "on"), &Value::from(false));
        assert_eq!(get(&calls[2].1[1], "buf"), &Value::from(3));
        assert_eq!(get(&calls[2].1[1], "max_bytes"), &Value::from(4_194_304));
        assert_eq!(get(&calls[2].1[1], "max_lines"), &Value::from(50_000));
        assert_eq!(get(&calls[3].1[1], "line"), &Value::from(50));
        assert_eq!(get(&calls[3].1[1], "token"), &Value::from(7));
        assert_eq!(field(&calls[3].1[1], "cursor"), None, "no cursor unless one was given");
        assert_eq!(get(&calls[7].1[1], "cursor"), &Value::from(54));
        assert_eq!(field(&calls[3].1[1], "row"), None, "no row unless one was given");
        assert_eq!(get(&calls[7].1[1], "row"), &Value::from(9));
        assert_eq!(get(&calls[4].1[1], "line"), &Value::from(5));
        assert_eq!(get(&calls[4].1[1], "col"), &Value::from(0));
        assert!(
            field(&calls[6].1[1], "scratch_dir").is_none(),
            "no scratch directory, no key"
        );
    }

    #[test]
    fn follow_answers_decode_and_a_malformed_one_says_what_is_missing() {
        let m = |pairs: &[(&str, Value)]| Value::Map(pairs.iter().map(|(k, v)| (Value::from(*k), v.clone())).collect());
        let empty = Value::Map(Vec::new());
        assert_eq!(
            read_installed(&m(&[
                ("ok", true.into()),
                ("skipped", Value::Array(vec!["EitriMarkdown".into()]))
            ])),
            Ok(Installed {
                skipped: vec!["EitriMarkdown".to_string()],
                early: false
            })
        );
        assert_eq!(
            read_installed(&m(&[("ok", true.into()), ("skipped", empty.clone())])),
            Ok(Installed {
                skipped: Vec::new(),
                early: false
            })
        );
        assert_eq!(
            read_installed(&m(&[("ok", false.into()), ("early", true.into())])),
            Ok(Installed {
                skipped: Vec::new(),
                early: true
            })
        );
        assert!(read_installed(&m(&[("ok", false.into())])).is_err());

        // A msgpack str holding invalid UTF-8, as nvim sends a line with a stray byte.
        let raw: &[u8] = &[0xa3, b'x', 0xff, b'y'];
        let invalid = rmpv::decode::read_value(&mut &raw[..]).unwrap();
        assert!(matches!(invalid, Value::String(_)));
        assert_eq!(
            read_text(&m(&[
                ("buf", 3.into()),
                ("tick", 41.into()),
                ("lines", Value::Array(vec!["# a".into(), invalid]))
            ])),
            Ok(PulledText::Lines {
                buf: 3,
                tick: 41,
                lines: vec!["# a".to_string(), "x\u{fffd}y".to_string()]
            })
        );
        assert_eq!(
            read_text(&m(&[("buf", 3.into()), ("tick", 41.into()), ("lines", empty.clone())])),
            Ok(PulledText::Lines {
                buf: 3,
                tick: 41,
                lines: Vec::new()
            })
        );
        assert_eq!(
            read_text(&m(&[
                ("buf", 3.into()),
                ("tick", 42.into()),
                ("too_large", true.into()),
                ("bytes", 5_452_595.into()),
                ("lines", 61_000.into())
            ])),
            Ok(PulledText::TooLarge {
                buf: 3,
                tick: 42,
                bytes: 5_452_595,
                lines: 61_000
            })
        );
        assert_eq!(
            read_text(&m(&[("gone", true.into()), ("buf", 3.into())])),
            Ok(PulledText::Gone { buf: 3 })
        );
        assert_eq!(read_applied(&m(&[("applied", false.into())])), Ok(false));
        assert!(read_text(&m(&[("tick", 1.into())])).unwrap_err().contains("buf"));
        assert!(read_applied(&empty).unwrap_err().contains("applied"));
    }

    #[test]
    fn cwd_calls_carry_arguments_and_order_and_answers_are_checked() {
        let rpc = Recorder::default();
        let _ = cwd(&rpc, "demo", Path::new("/work/back\\slash"));
        let _ = cwd_clear(&rpc, "demo");
        let calls = rpc.0.borrow();
        for (at, op) in ["cd", "clear"].iter().enumerate() {
            assert!(std::ptr::eq(calls[at].0, PLUGIN_CWD_LUA));
            assert_eq!(calls[at].1[0], Value::from(*op));
            assert_eq!(get(&calls[at].1[1], "owner"), &Value::from("demo"));
        }
        assert_eq!(
            get(&calls[0].1[1], "dir"),
            &Value::Binary(b"/work/back\\slash".to_vec())
        );
        assert_eq!(get(&calls[0].1[1], "maker"), get(&calls[1].1[1], "maker"));
        assert!(get(&calls[0].1[1], "order").as_u64() < get(&calls[1].1[1], "order").as_u64());
        let reply = Value::Map(vec![("dir".into(), "/work".into()), ("tab".into(), 1.into())]);
        assert_eq!(
            decode_cwd(&reply),
            Ok(ChangedCwd {
                dir: "/work".into(),
                tab: 1
            })
        );
        let clear = Value::Map(vec![("restored".into(), 2.into()), ("moved".into(), 1.into())]);
        assert_eq!(decode_cwd_clear(&clear), Ok(ClearedCwd { restored: 2, moved: 1 }));
        assert!(decode_cwd(&Value::Nil).unwrap_err().contains("dir"));
        assert!(decode_cwd(&clear).is_err());
        assert!(decode_cwd_clear(&reply).unwrap_err().contains("restored"));
    }

    #[test]
    fn the_snippets_keep_the_companion_contract() {
        for (name, lua) in [
            ("open", PLUGIN_OPEN_LUA),
            ("assist", PLUGIN_ASSIST_LUA),
            ("follow", PLUGIN_FOLLOW_LUA),
            ("cwd", PLUGIN_CWD_LUA),
        ] {
            assert!(lua.is_ascii(), "{name}");
            for banned in ["goto ", "vim.env", "EITRI_", "vim.cmd("] {
                assert!(!lua.contains(banned), "{name} holds {banned:?}");
            }
            assert!(lua.contains("= ..."), "{name} takes its options as arguments");
        }
    }
}
