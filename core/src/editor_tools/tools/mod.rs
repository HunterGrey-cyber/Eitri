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
pub mod mention_reads;
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
    mention_reads::BUFFERS_LUA,
    mention_reads::QUICKFIX_LUA,
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

    /// Every tool that may read or show a file outside the project, with arguments naming `path` and how its
    /// refusal of a place the card did not name ends.
    fn outside_tools(path: &str) -> [(&'static str, serde_json::Value, &'static str); 4] {
        [
            ("open_file", serde_json::json!({ "path": path }), "nothing was shown"),
            ("read_buffer", serde_json::json!({ "path": path }), "nothing was read"),
            ("diagnostics", serde_json::json!({ "path": path }), "nothing was read"),
            (
                "usages",
                serde_json::json!({ "path": path, "line": 1, "symbol": "outside" }),
                "nothing was read",
            ),
        ]
    }

    /// The steps of `tool` asked about `link.rs`, a project link to an outside file, in Manual mode, with
    /// `meanwhile` run while the outside card waits and the card then allowed.
    fn after_the_card(
        project: &Path,
        tool: &str,
        args: &serde_json::Value,
        meanwhile: &mut dyn FnMut(),
    ) -> Vec<super::super::step::Step> {
        use super::super::cards::{CardDecision, CardOrigin};
        use super::super::memory::Memory;
        use super::super::step::{AgentCall, EditorFailure, EditorReach, EditorSight, ToolCx};
        use super::super::testing::{clock::Clock, drive_with_cards};

        let _clock = Clock::new();
        let memory = Memory::default();
        let cx = ToolCx {
            mode: agent::PermissionMode::Manual,
            trust: agent::setting_sources::ProjectTrust::Trusted,
            root: project,
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        let run = tool_named(tool).unwrap().begin(args, &cx).unwrap();
        drive_with_cards(
            run,
            &cx,
            |_| Err(EditorFailure::NotAnswered),
            |_| {
                meanwhile();
                (CardDecision::Allow, CardOrigin::Key)
            },
        )
    }

    /// What became of a run: `None` when it reached the editor, else its refusal.
    fn refusal(steps: &[super::super::step::Step]) -> Option<String> {
        use super::super::step::Step;
        if steps.iter().any(|step| matches!(step, Step::Call { .. })) {
            return None;
        }
        match steps.last() {
            Some(Step::Done(result)) if result.is_error => Some(result.text.clone()),
            other => panic!("neither reached the editor nor refused: {other:?}"),
        }
    }

    /// The four tools recheck an outside card's place alike: one of them refusing what another lets through would
    /// read an unapproved file, or refuse an approved one.
    #[test]
    fn every_outside_tool_rechecks_the_card_alike() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let scratch = crate::test_scratch_dir::ScratchDir::new("editor-tools-outside-recheck", "alike");
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&beside).unwrap();
        let named = beside.join("b.rs");
        let other = beside.join("other.rs");
        let alias = beside.join("alias.rs");
        fs::write(&named, "named\n").unwrap();
        fs::write(&other, "other\n").unwrap();
        symlink(&named, &alias).unwrap();
        let link = root.join("link.rs");
        let point = |at: &Path| {
            let _ = fs::remove_file(&link);
            symlink(at, &link).unwrap();
        };

        for (tool, args, nothing) in outside_tools("link.rs") {
            // Left alone, the card's place is what the tool uses.
            point(&named);
            assert_eq!(refusal(&after_the_card(&root, tool, &args, &mut || {})), None, "{tool}");

            // Led elsewhere while the card waited: refused, whatever the tool.
            point(&named);
            let moved = refusal(&after_the_card(&root, tool, &args, &mut || point(&other)));
            let refused = format!("link.rs now leads somewhere other than {}; {nothing}", named.display());
            assert_eq!(moved, Some(refused), "{tool}");

            // Led to the same file by another way: still the place the card named.
            point(&named);
            let relinked = refusal(&after_the_card(&root, tool, &args, &mut || point(&alias)));
            assert_eq!(relinked, None, "{tool}");

            // Gone while the card waited.
            point(&named);
            let gone = refusal(&after_the_card(&root, tool, &args, &mut || {
                fs::remove_file(&link).unwrap()
            }));
            assert_eq!(gone.as_deref(), Some("no such file: link.rs"), "{tool}");
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
