//! The tools, one file pair each: `<name>.rs` holds the row, the validation and the run; `<name>.lua` the
//! chunk. A chunk is a constant, every value a msgpack argument `(stamp, args)`, and it starts, waits for
//! nothing and answers one table.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rmpv::Value;

use super::step::Tool;

/// A tool's chunk: the deadline and sequence check, the shared buffer lookup, the body. A path in here is
/// relative to the file that uses the macro, so a chunk is built from a file of this directory.
macro_rules! chunk {
    ($body:literal) => {
        concat!(
            include_str!("../prelude.lua"),
            "\n",
            include_str!("../../find_buf.lua"),
            "\n",
            include_str!($body)
        )
    };
}

/// [`chunk!`] for a tool that loads a file: `load_file` after the lookup it needs, ahead of the body.
macro_rules! chunk_loading {
    ($body:literal) => {
        concat!(
            include_str!("../prelude.lua"),
            "\n",
            include_str!("../../find_buf.lua"),
            "\n",
            include_str!("load_file.lua"),
            "\n",
            include_str!($body)
        )
    };
}

pub mod apply_code_action;
pub mod apply_proposal;
pub mod baseline;
pub mod diagnostics;
pub mod editor_state;
pub mod format;
pub mod hover;
pub mod lsp;
pub mod lsp_code_actions;
pub mod lsp_rename;
pub mod nvim_exec;
pub mod open_file;
pub mod read_buffer;
pub mod symbols;
pub mod usages;

/// Every tool of this build. A tool's off switch and its place in a session's list come from its row here.
pub static TOOLS: &[&dyn Tool] = &[
    &open_file::OpenFile,
    &editor_state::EditorState,
    &read_buffer::ReadBuffer,
    &diagnostics::Diagnostics,
    &nvim_exec::NvimExec,
    &usages::Usages,
    &hover::Hover,
    &symbols::Symbols,
    &lsp_rename::LspRename,
    &format::Format,
    &lsp_code_actions::LspCodeActions,
    &apply_code_action::ApplyCodeAction,
    &apply_proposal::ApplyProposal,
];

/// Every chunk of this build, for the tests that hold each to the stamp rule. The exec chunk is the one left out: it is
/// named only in its own file, whose tests hold it to the same rule.
pub const CHUNKS: &[&str] = &[
    open_file::OPEN_FILE_LUA,
    editor_state::EDITOR_STATE_LUA,
    read_buffer::READ_BUFFER_LUA,
    diagnostics::DIAGNOSTICS_LUA,
    lsp::LSP_ASK_LUA,
    lsp::LSP_COLLECT_LUA,
];

pub fn tool_named(name: &str) -> Option<&'static dyn Tool> {
    TOOLS.iter().copied().find(|tool| tool.row().name == name)
}

/// A path as a chunk's argument: msgpack bin, since a file name need not be text.
fn path_value(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chunk_opens_with_the_stamp_check() {
        let prelude = include_str!("../prelude.lua");
        for chunk in CHUNKS {
            assert!(chunk.starts_with(prelude));
        }
    }

    #[test]
    fn every_chunk_compiles() {
        let lua = mlua::Lua::new();
        for chunk in CHUNKS {
            lua.load(*chunk).into_function().expect("Lua source");
        }
    }

    #[test]
    fn a_tool_is_found_by_its_row_name() {
        for tool in TOOLS {
            let name = tool.row().name;
            assert_eq!(tool_named(name).map(|found| found.row().name), Some(name));
        }
        assert!(tool_named("nvim_nope").is_none());
    }
}
