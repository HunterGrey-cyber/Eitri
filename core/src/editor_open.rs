//! Opens a file another process asked this window to open, in the window's running editor.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rmpv::Value;

use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

/// A `pub static`, so tests can compare it by address; `core/tests/editor_rpc_scan.rs` accepts the call as it is.
pub static EDITOR_OPEN_LUA: &str = include_str!("editor_open.lua");

/// Asks the editor to open `path`, at `line` and `col` when given. `line` and `col` count from one, `col` in bytes;
/// a line past the end is the last line. The answer is `{how = edit | split | current, line, col}`, with `col` from
/// zero, or an error when the file did not open.
pub fn open(rpc: &dyn EditorRpc, path: &Path, line: Option<u32>, col: Option<u32>) -> Pending {
    // Bytes, never re-encoded: nvim gives Lua a string either way.
    let mut opts = vec![(Value::from("path"), Value::Binary(path.as_os_str().as_bytes().to_vec()))];
    if let Some(line) = line {
        opts.push((Value::from("line"), Value::from(line)));
    }
    if let Some(col) = col {
        opts.push((Value::from("col"), Value::from(col)));
    }
    rpc.exec_lua(EDITOR_OPEN_LUA, vec![Value::Map(opts)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim_rpc::RpcError;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Calls(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Calls {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::failed(RpcError::Unavailable("no editor in this test".into()))
        }
        fn target(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn every_value_is_an_argument_of_the_one_constant() {
        let rpc = Calls::default();
        open(&rpc, Path::new("/p/a %#b.rs"), Some(4_294_967_295), Some(7));
        let calls = rpc.0.borrow();
        assert_eq!(calls.len(), 1);
        assert!(std::ptr::eq(calls[0].0, EDITOR_OPEN_LUA));
        assert_eq!(
            calls[0].1,
            vec![Value::Map(vec![
                (Value::from("path"), Value::Binary(b"/p/a %#b.rs".to_vec())),
                (Value::from("line"), Value::from(4_294_967_295_u32)),
                (Value::from("col"), Value::from(7_u32)),
            ])]
        );
    }

    #[test]
    fn a_line_or_column_not_given_is_left_out() {
        let rpc = Calls::default();
        open(&rpc, Path::new("/p/a.rs"), None, None);
        let calls = rpc.0.borrow();
        let Value::Map(opts) = &calls[0].1[0] else {
            panic!("options are one map")
        };
        assert_eq!(opts.len(), 1, "an absent value never reaches nvim as vim.NIL");
    }

    #[test]
    fn the_chunk_takes_its_options_as_arguments_and_builds_no_ex_text() {
        assert!(EDITOR_OPEN_LUA.is_ascii());
        assert!(EDITOR_OPEN_LUA.contains("= ..."));
        for banned in [
            "goto ",
            "vim.env",
            "EITRI_",
            "vim.cmd(",
            "vim.cmd.",
            "nvim_command",
            "nvim_exec",
        ] {
            assert!(!EDITOR_OPEN_LUA.contains(banned), "the chunk holds {banned:?}");
        }
    }
}
