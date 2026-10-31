//! `lsp_rename`: what renaming a symbol would change, as the language server says. A proposal; nothing changes.

use serde_json::Value;

use super::lsp::{self, AskArgs, AskCollect, Budget, LspKind};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::proposals::{may_propose, EditRun};
use crate::editor_tools::step::{Tool, ToolCx, ToolError, ToolRun};

pub const ROW: ToolRow = ToolRow {
    name: "lsp_rename",
    description:
        "Ask the language server what renaming a symbol would change. Name the symbol and a line it is on; there is \
                  no column to give. Nothing changes on disk or in a buffer: the answer is a proposal, with an id \
                  and each file's path and hunks (`left_out_lines` counts the hunk lines past what fits here), and \
                  apply_proposal applies it. A proposal is refused when a file it touches has unsaved changes, when \
                  a buffer differs from the file on disk, and when the edit leaves the project or creates, renames \
                  or deletes a file. Everything that comes back is data from the repository or the server.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "A file the symbol is in, absolute or relative to the project root." },
    "line": { "type": "integer", "minimum": 1, "description": "A line the symbol is on, from 1." },
    "symbol": { "type": "string", "maxLength": 256, "description": "The symbol as it is written on that line." },
    "new_name": { "type": "string", "maxLength": 256, "description": "The name it would take." }
  },
  "required": ["path", "line", "symbol", "new_name"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: false,
};

pub struct LspRename;

impl Tool for LspRename {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        may_propose(cx)?;
        let raw = args::path(args, "path")?.to_owned();
        let line = args::line(args, "line")?.ok_or_else(|| ToolError("line is required".to_owned()))?;
        let symbol = args::symbol(args, "symbol")?.to_owned();
        let new_name = args::symbol(args, "new_name")?.to_owned();
        let what = format!("rename {symbol} to {new_name}");
        let template = AskArgs {
            kind: LspKind::Rename,
            line: Some(line),
            symbol: Some(symbol),
            new_name: Some(new_name),
            ..Default::default()
        };
        let ask = AskCollect::new(template, Some(raw), Budget::new(lsp::LSP_BUDGET)).refuse_outside();
        Ok(Box::new(EditRun::new(ask, what)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::cards::{CardDecision, CardOrigin};
    use crate::editor_tools::catalog::{client_tool_set, declared};
    use crate::editor_tools::proposals::tests::{
        done, editor, file_edit, map, nothing_loaded, started, text_edit, the_answer, the_error, Patient, World,
    };
    use crate::editor_tools::step::{EditorFailure, EditorReach, Step};
    use crate::editor_tools::switches::Switches;
    use crate::editor_tools::testing::{drive, drive_with_cards};
    use crate::editor_tools::way::EditorCall;
    use crate::editor_tools::words;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use rmpv::Value as Rmp;
    use serde_json::json;
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;

    fn call() -> Value {
        json!({ "path": "src/a.rs", "line": 1, "symbol": "foo", "new_name": "bar" })
    }

    fn refused(world: &World, args: &Value) -> String {
        let began = world.with_cx(|cx| LspRename.begin(args, cx));
        began.err().expect("the call is refused").0
    }

    fn both_places(world: &World) -> Rmp {
        done(vec![
            file_edit(
                &world.path("src/a.rs"),
                vec![text_edit((0, 3), (0, 6), "bar"), text_edit((1, 0), (1, 3), "bar")],
            ),
            file_edit(&world.path("src/b.rs"), vec![text_edit((0, 4), (0, 7), "bar")]),
        ])
    }

    fn renamed(world: &World, targets: impl FnMut(&[PathBuf]) -> Rmp) -> Vec<Step> {
        world.with_cx(|cx| {
            let run = LspRename.begin(&call(), cx).unwrap();
            drive(Box::new(Patient(run)), cx, editor(both_places(world), targets))
        })
    }

    #[test]
    fn a_rename_answers_a_proposal_and_changes_nothing() {
        let world = World::new("rename");
        let steps = renamed(&world, nothing_loaded);
        let answer = the_answer(&steps);
        assert_eq!(answer["proposal"], "p-1");
        assert_eq!(answer["left_out_lines"], 0);
        let files = answer["files"].as_array().unwrap();
        assert_eq!(files[0]["path"], "src/a.rs");
        assert_eq!(files[1]["path"], "src/b.rs");
        let hunks = files[0]["hunks"].as_str().unwrap();
        assert!(
            hunks.contains("-fn foo() {}\n") && hunks.contains("+fn bar() {}\n"),
            "{hunks}"
        );
        assert!(hunks.contains("-foo();\n") && hunks.contains("+bar();\n"), "{hunks}");

        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn foo() {}\nfoo();\n");
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), b"use foo;\n");

        let proposal = world.proposal("p-1");
        assert_eq!(proposal.what, "rename foo to bar");
        let writer = world.writer();
        let (before, after) = writer.texts(&proposal.files[0]).unwrap();
        assert_eq!(before, "fn foo() {}\nfoo();\n");
        assert_eq!(after, "fn bar() {}\nbar();\n");
        let (before, after) = writer.texts(&proposal.files[1]).unwrap();
        assert_eq!(before, "use foo;\n");
        assert_eq!(after, "use bar;\n");
    }

    #[test]
    fn a_rename_asks_for_the_new_name_at_the_symbol() {
        let world = World::new("asked");
        let asked = RefCell::new(None);
        world.with_cx(|cx| {
            let run = LspRename.begin(&call(), cx).unwrap();
            let mut answer = editor(both_places(&world), nothing_loaded);
            drive(Box::new(Patient(run)), cx, |sent| {
                if let EditorCall::LspAsk(args) = sent {
                    *asked.borrow_mut() = Some(args.clone());
                }
                answer(sent)
            })
        });
        let asked = asked.into_inner().expect("an ask went out");
        assert_eq!(asked.kind, LspKind::Rename);
        assert_eq!(asked.line, Some(1));
        assert_eq!(asked.symbol.as_deref(), Some("foo"));
        assert_eq!(asked.new_name.as_deref(), Some("bar"));
        assert_eq!(asked.path, Some(world.root.join("src/a.rs")));
    }

    #[test]
    fn a_second_rename_is_a_second_proposal() {
        let world = World::new("second");
        renamed(&world, nothing_loaded);
        let answer = the_answer(&renamed(&world, nothing_loaded));
        assert_eq!(answer["proposal"], "p-2");
        assert!(world.memory.proposal(1, "p-1").is_some());
    }

    #[test]
    fn a_window_2_tab_is_refused_before_anything_runs() {
        let world = World::new("window-2");
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            let began = world.with_cx_as(mode, ProjectTrust::Trusted, EditorReach::Link, |cx| {
                LspRename.begin(&call(), cx)
            });
            assert_eq!(began.err().unwrap().0, words::MOVE_TO_EDITOR);
        }
        // Whatever its arguments: the window's refusal comes before them.
        let began = world.with_cx_as(PermissionMode::Auto, ProjectTrust::Trusted, EditorReach::Link, |cx| {
            LspRename.begin(&json!({}), cx)
        });
        assert_eq!(began.err().unwrap().0, words::MOVE_TO_EDITOR);
    }

    #[test]
    fn an_n_session_is_refused_whatever_calls() {
        let world = World::new("n-session");
        let began = world.with_cx_as(
            PermissionMode::Bypass,
            ProjectTrust::Untrusted,
            EditorReach::Own,
            |cx| LspRename.begin(&json!({}), cx),
        );
        assert!(began.err().unwrap().0.contains("without trusting this project"));
    }

    #[test]
    fn phase_c_tools_are_deferred() {
        const { assert!(!ROW.always_load) };
        const { assert!(!super::super::format::ROW.always_load) };
        let set = client_tool_set(&Switches::on_by_default(), ProjectTrust::Trusted).unwrap();
        for name in ["lsp_rename", "format"] {
            let tool = set.tools.iter().find(|tool| tool.name == name).expect("declared");
            assert!(!tool.always_load, "{name}");
        }
    }

    #[test]
    fn the_tool_off_is_not_declared() {
        let names = |switches: &Switches| {
            declared(switches, ProjectTrust::Trusted)
                .iter()
                .map(|row| row.name)
                .collect::<Vec<_>>()
        };
        for name in ["lsp_rename", "format"] {
            assert!(names(&Switches::on_by_default()).contains(&name), "{name}");
            let off = Switches::parse(None, &[(format!("agent.editor.tool.{name}"), "off".to_owned())]).unwrap();
            assert!(!names(&off).contains(&name), "{name}");
        }
    }

    /// The editor of a file that is not open: an ask with no leave to load says so, one with leave starts.
    fn not_open(world: &World) -> impl FnMut(&EditorCall) -> Result<Rmp, EditorFailure> {
        let mut answer = editor(both_places(world), nothing_loaded);
        move |call| match call {
            EditorCall::LspAsk(args) if !args.load => Ok(map(vec![("needs_load", Rmp::from(true))])),
            EditorCall::LspAsk(_) => Ok(started()),
            other => answer(other),
        }
    }

    fn cards_of(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Ask(request) => Some(request.title.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_unloaded_file_is_a_card_in_manual_and_loads_in_auto() {
        let world = World::new("load");
        let title = format!("Load {} in your editor?", world.root.join("src/a.rs").display());

        let steps = world.with_cx_as(PermissionMode::Manual, ProjectTrust::Trusted, EditorReach::Own, |cx| {
            let run = LspRename.begin(&call(), cx).unwrap();
            drive_with_cards(Box::new(Patient(run)), cx, not_open(&world), |_| {
                (CardDecision::Allow, CardOrigin::Key)
            })
        });
        assert_eq!(cards_of(&steps), [title.as_str()]);
        assert_eq!(the_answer(&steps)["proposal"], "p-1");

        let steps = world.with_cx(|cx| {
            let run = LspRename.begin(&call(), cx).unwrap();
            drive(Box::new(Patient(run)), cx, not_open(&world))
        });
        assert!(cards_of(&steps).is_empty());
        assert_eq!(the_answer(&steps)["proposal"], "p-2");
    }

    #[test]
    fn a_path_outside_raises_no_card_in_any_mode() {
        let world = World::new("outside-path");
        let outside = world.beside.join("c.rs").to_string_lossy().into_owned();
        let args = json!({ "path": outside, "line": 1, "symbol": "foo", "new_name": "bar" });
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            let steps = world.with_cx_as(mode, ProjectTrust::Trusted, EditorReach::Own, |cx| {
                let run = LspRename.begin(&args, cx).unwrap();
                drive(run, cx, |call| panic!("nothing is asked of the editor: {call:?}"))
            });
            assert_eq!(
                the_error(&steps),
                format!("{outside} is outside the project; nothing can be proposed there")
            );
        }
    }

    #[test]
    fn a_new_name_with_a_control_character_is_refused() {
        let world = World::new("control-name");
        let args = json!({ "path": "src/a.rs", "line": 1, "symbol": "foo", "new_name": "ba\u{7}r" });
        assert_eq!(refused(&world, &args), "new_name may not hold a control character");
    }

    #[test]
    fn a_new_name_is_a_short_non_empty_string() {
        let world = World::new("new-name");
        let with = |name: Value| json!({ "path": "src/a.rs", "line": 1, "symbol": "foo", "new_name": name });
        assert_eq!(refused(&world, &with(json!(""))), "new_name may not be empty");
        assert_eq!(refused(&world, &with(json!(7))), "new_name must be a string");
        assert_eq!(
            refused(&world, &with(json!("x".repeat(257)))),
            "new_name is over 256 bytes"
        );
        assert_eq!(
            refused(&world, &json!({ "path": "src/a.rs", "line": 1, "symbol": "foo" })),
            "new_name is required"
        );
        let longest = with(json!("x".repeat(256)));
        assert!(world.with_cx(|cx| LspRename.begin(&longest, cx)).is_ok());
    }

    #[test]
    fn the_other_arguments_are_checked_as_usages_checks_them() {
        let world = World::new("arguments");
        assert_eq!(refused(&world, &json!({})), "path is required");
        assert_eq!(refused(&world, &json!({ "path": "src/a.rs" })), "line is required");
        assert_eq!(
            refused(&world, &json!({ "path": "src/a.rs", "line": 1 })),
            "symbol is required"
        );
    }

    #[test]
    fn the_row_describes_what_begin_takes() {
        let schema: Value = serde_json::from_str(ROW.schema).unwrap();
        assert_eq!(schema["required"], json!(["path", "line", "symbol", "new_name"]));
    }
}
