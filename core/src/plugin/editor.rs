//! The two fixed Lua chunks a plugin's host runs in the user's editor, and how their answers are read. The
//! Lua is a compile-time constant and every value -- a path, a line, a setting -- travels as an argument,
//! so nothing a page sends can become Lua or Ex source in the user's nvim.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rmpv::Value;

use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

// Statics, not consts: a const is copied to each use, and the tests check that the very same text is what
// reaches the editor, by address.
pub static PLUGIN_OPEN_LUA: &str = include_str!("plugin_open.lua");
pub static PLUGIN_ASSIST_LUA: &str = include_str!("plugin_assist.lua");

/// A path as bytes: nvim gives Lua a string either way, and bytes are never re-encoded.
fn path_value(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

pub fn open(rpc: &dyn EditorRpc, path: &Path, line: Option<u32>, col: Option<u32>) -> Pending {
    let mut opts = vec![(Value::from("path"), path_value(path))];
    if let Some(line) = line {
        opts.push((Value::from("line"), Value::from(line)));
    }
    if let Some(col) = col {
        opts.push((Value::from("col"), Value::from(col)));
    }
    rpc.exec_lua(PLUGIN_OPEN_LUA, vec![Value::Map(opts)])
}

pub fn assist_apply(rpc: &dyn EditorRpc, owner: &str, root: &Path, diagnostics: bool, inlay_hints: bool) -> Pending {
    let opts = Value::Map(vec![
        (Value::from("owner"), Value::from(owner)),
        (Value::from("root"), path_value(root)),
        (Value::from("diagnostics"), Value::from(diagnostics)),
        (Value::from("inlay_hints"), Value::from(inlay_hints)),
    ]);
    rpc.exec_lua(PLUGIN_ASSIST_LUA, vec![Value::from("apply"), opts])
}

pub fn assist_clear(rpc: &dyn EditorRpc, owner: &str) -> Pending {
    let opts = Value::Map(vec![(Value::from("owner"), Value::from(owner))]);
    rpc.exec_lua(PLUGIN_ASSIST_LUA, vec![Value::from("clear"), opts])
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

    #[test]
    fn the_snippets_keep_the_companion_contract() {
        for (name, lua) in [("open", PLUGIN_OPEN_LUA), ("assist", PLUGIN_ASSIST_LUA)] {
            assert!(lua.is_ascii(), "{name}");
            for banned in ["goto ", "vim.env", "EITRI_", "vim.cmd("] {
                assert!(!lua.contains(banned), "{name} holds {banned:?}");
            }
            assert!(lua.contains("= ..."), "{name} takes its options as arguments");
        }
    }
}
