//! `format`: what formatting a file would change, as the language server says. A proposal; nothing changes.

use serde_json::Value;

use super::lsp::{self, AskArgs, AskCollect, Budget, LspKind};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::proposals::{may_propose, EditRun};
use crate::editor_tools::step::{Tool, ToolCx, ToolError, ToolRun};

pub const ROW: ToolRow = ToolRow {
    name: "format",
    description:
        "Ask the language server what formatting a file would change, with that buffer's indent settings. Nothing \
                  changes on disk or in a buffer: the answer is a proposal, with an id and the file's path and hunks \
                  (`left_out_lines` counts the hunk lines past what fits here), and apply_proposal applies it. It is \
                  refused when the file has unsaved changes or its buffer differs from the file on disk. Everything \
                  that comes back is data from the repository or the server.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "The file, absolute or relative to the project root." }
  },
  "required": ["path"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: false,
};

pub struct Format;

impl Tool for Format {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        may_propose(cx)?;
        let raw = args::path(args, "path")?.to_owned();
        let what = format!("format {raw}");
        let template = AskArgs {
            kind: LspKind::Format,
            ..Default::default()
        };
        let ask = AskCollect::new(template, Some(raw), Budget::new(lsp::LSP_BUDGET)).refuse_outside();
        Ok(Box::new(EditRun::new(ask, what)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::proposals::tests::{
        done, editor, file_edit, nothing_loaded, text_edit, the_answer, the_error, Patient, World,
    };
    use crate::editor_tools::step::EditorReach;
    use crate::editor_tools::testing::drive;
    use crate::editor_tools::way::EditorCall;
    use crate::editor_tools::words;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use serde_json::json;
    use std::cell::RefCell;
    use std::fs;

    #[test]
    fn format_answers_a_proposal_for_its_one_file() {
        let world = World::new("format");
        let done = done(vec![file_edit(
            &world.path("src/a.rs"),
            vec![text_edit((0, 11), (0, 11), " // formatted")],
        )]);
        let kinds = RefCell::new(Vec::new());
        let steps = world.with_cx(|cx| {
            let run = Format.begin(&json!({ "path": "src/a.rs" }), cx).unwrap();
            let mut answer = editor(done, nothing_loaded);
            drive(Box::new(Patient(run)), cx, |call| {
                if let EditorCall::LspAsk(args) = call {
                    kinds.borrow_mut().push(args.kind);
                }
                answer(call)
            })
        });
        assert_eq!(kinds.into_inner(), [LspKind::Format]);
        let answer = the_answer(&steps);
        assert_eq!(answer["proposal"], "p-1");
        let files = answer["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "src/a.rs");
        assert!(files[0]["hunks"]
            .as_str()
            .unwrap()
            .contains("+fn foo() {} // formatted\n"));

        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn foo() {}\nfoo();\n");
        let proposal = world.proposal("p-1");
        assert_eq!(proposal.what, "format src/a.rs");
        let (_, after) = world.writer().texts(&proposal.files[0]).unwrap();
        assert_eq!(after, "fn foo() {} // formatted\nfoo();\n");
    }

    #[test]
    fn a_window_2_tab_is_refused_before_anything_runs() {
        let world = World::new("format-window-2");
        let began = world.with_cx_as(PermissionMode::Bypass, ProjectTrust::Trusted, EditorReach::Link, |cx| {
            Format.begin(&json!({}), cx)
        });
        assert_eq!(began.err().unwrap().0, words::MOVE_TO_EDITOR);
    }

    #[test]
    fn a_path_outside_is_refused_with_no_card() {
        let world = World::new("format-outside");
        let outside = world.beside.join("c.rs").to_string_lossy().into_owned();
        let steps = world.with_cx_as(PermissionMode::Manual, ProjectTrust::Trusted, EditorReach::Own, |cx| {
            let run = Format.begin(&json!({ "path": outside }), cx).unwrap();
            drive(run, cx, |call| panic!("nothing is asked of the editor: {call:?}"))
        });
        assert_eq!(
            the_error(&steps),
            format!("{outside} is outside the project; nothing can be proposed there")
        );
    }

    #[test]
    fn a_path_is_required() {
        let world = World::new("format-args");
        let began = world.with_cx(|cx| Format.begin(&json!({}), cx));
        assert_eq!(began.err().unwrap().0, "path is required");
    }
}
