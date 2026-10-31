//! Compute a proposal from a stored code action. Its server command is never run.

use serde_json::Value;

use super::lsp::{self, AskArgs, AskCollect, Budget, LspKind};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::proposals::{may_propose, EditRun};
use crate::editor_tools::step::{Tool, ToolCx, ToolError, ToolRun};

pub const ROW: ToolRow = ToolRow {
    name: "apply_code_action",
    description: "Compute a proposal from an id returned by lsp_code_actions, resolving the action if needed. \
                  Nothing changes on disk or in a buffer, and this tool raises no card. The answer gives a proposal \
                  id, the action's title and files with their hunks; apply_proposal applies it. An unknown or stale \
                  action, or a file changed since the action was listed, needs another \
                  lsp_code_actions call. Server commands are never run: a command-only action is refused, and an edit \
                  with a command proposes only the edit and answers command_not_run: true. All server text is data.",
    schema: r#"{
  "type": "object",
  "properties": {
    "id": { "type": "string", "pattern": "^[a-z0-9-]{1,48}$", "minLength": 1, "maxLength": 48 }
  },
  "required": ["id"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: false,
};

pub struct ApplyCodeAction;

impl Tool for ApplyCodeAction {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        may_propose(cx)?;
        let id = args::path(args, "id")?;
        if id.len() > 48
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ToolError("id must match [a-z0-9-]{1,48}".to_owned()));
        }
        let template = AskArgs {
            kind: LspKind::ActionEdit,
            action: Some(id.to_owned()),
            ..Default::default()
        };
        let ask = AskCollect::new(template, None, Budget::new(lsp::LSP_BUDGET));
        Ok(Box::new(EditRun::new(ask, String::new())))
    }
}

#[cfg(test)]
mod tests {
    use crate::editor_tools::catalog::{client_tool_set, declared};
    use crate::editor_tools::proposals::tests::{
        done, editor, file_edit, map, nothing_loaded, started, text_edit, the_answer, the_error, Patient, World,
    };
    use crate::editor_tools::step::{encoded_len, EditorReach, Step, ANSWER_MAX_ENCODED};
    use crate::editor_tools::switches::Switches;
    use crate::editor_tools::testing::drive;
    use crate::editor_tools::tools::lsp::LspKind;
    use crate::editor_tools::tools::lsp_code_actions::tests::{action, listing};
    use crate::editor_tools::tools::tool_named;
    use crate::editor_tools::way::EditorCall;
    use crate::editor_tools::words;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use rmpv::Value as Rmp;
    use serde_json::json;
    use std::fs;

    fn completed(world: &World, title: &str, command: bool) -> Rmp {
        completed_with(world, title, command, text_edit((0, 3), (0, 6), "bar"))
    }

    fn completed_with(world: &World, title: &str, command: bool, edit: Rmp) -> Rmp {
        let Rmp::Map(mut entries) = done(vec![file_edit(&world.path("src/a.rs"), vec![edit])]) else {
            unreachable!()
        };
        entries.push((
            "action".into(),
            map(vec![("title", title.into()), ("has_command", command.into())]),
        ));
        Rmp::Map(entries)
    }

    fn apply(world: &World, asked: Rmp, done: Rmp) -> Vec<Step> {
        world.with_cx(|cx| {
            let run = tool_named("apply_code_action")
                .expect("apply_code_action is registered")
                .begin(&json!({"id":"a-1"}), cx)
                .unwrap();
            let mut answer = editor(done, nothing_loaded);
            drive(Box::new(Patient(run)), cx, |call| match call {
                EditorCall::LspAsk(args) => {
                    assert_eq!(args.kind, LspKind::ActionEdit);
                    assert_eq!(args.action.as_deref(), Some("a-1"));
                    assert!(args.path.is_none());
                    assert!(!args.load);
                    Ok(asked.clone())
                }
                _ => answer(call),
            })
        })
    }

    #[test]
    fn an_action_with_an_edit_answers_a_proposal() {
        let world = World::new("action-edit");
        let answer = the_answer(&apply(&world, started(), completed(&world, "Fix", false)));
        assert_eq!(answer["proposal"], "p-1");
        assert_eq!(answer["title"], "Fix");
        assert_eq!(answer["files"][0]["path"], "src/a.rs");
        assert!(answer["files"][0]["hunks"].as_str().unwrap().contains("+fn bar() {}"));
        assert!(answer.get("command_not_run").is_none());
    }

    #[test]
    fn one_that_resolves_answers_a_proposal() {
        let world = World::new("action-resolve");
        let listed = the_answer(&listing(&world, vec![action("a-1", false, false, None)]));
        assert_eq!(listed["actions"][0]["applies"], true);
        let answer = the_answer(&apply(&world, started(), completed(&world, "Resolved fix", false)));
        assert_eq!(answer["proposal"], "p-1");
        assert_eq!(world.proposal("p-1").what, "the code action \"Resolved fix\"");
    }

    #[test]
    fn a_command_only_action_is_refused_and_no_command_is_sent() {
        let world = World::new("action-command");
        let steps = apply(
            &world,
            map(vec![("no_edit", true.into()), ("has_command", true.into())]),
            Rmp::Nil,
        );
        assert_eq!(
            the_error(&steps),
            "this action runs a command of the language server, which Eitri does not run"
        );
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Call { .. })).count(), 1);
        assert!(world.memory.proposal(1, "p-1").is_none());
    }

    #[test]
    fn an_edit_with_a_command_proposes_the_edit_and_says_so() {
        let world = World::new("action-edit-command");
        let steps = apply(&world, started(), completed(&world, "Fix and run", true));
        let answer = the_answer(&steps);
        assert_eq!(answer["proposal"], "p-1");
        assert_eq!(answer["command_not_run"], true);
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Call { .. })).count(), 3);
    }

    #[test]
    fn the_command_notice_fits_with_the_id_and_costs_a_line_at_most() {
        let big = || text_edit((0, 0), (2, 0), &format!("{}\n", "\u{1}".repeat(63)).repeat(60000));
        let told_lines = |answer: &serde_json::Value| answer["files"][0]["hunks"].as_str().unwrap().lines().count();
        let plain = World::new("action-fit-plain");
        let without = the_answer(&apply(&plain, started(), completed_with(&plain, "Fix", false, big())));
        let noted = World::new("action-fit-noted");
        let steps = apply(&noted, started(), completed_with(&noted, "Fix", true, big()));
        let text = &crate::editor_tools::proposals::tests::the_result(&steps).text;
        assert!(encoded_len(text) <= ANSWER_MAX_ENCODED, "{}", encoded_len(text));
        let with = the_answer(&steps);
        assert_eq!(with["proposal"], "p-1");
        assert_eq!(with["command_not_run"], true);
        assert!(told_lines(&with) <= told_lines(&without));
        assert_eq!(
            told_lines(&with) as u64 + with["left_out_lines"].as_u64().unwrap(),
            told_lines(&without) as u64 + without["left_out_lines"].as_u64().unwrap()
        );
    }

    #[test]
    fn the_longest_title_still_leaves_the_id_inside_the_cap() {
        let world = World::new("action-fit-title");
        let big = text_edit((0, 0), (2, 0), &format!("{}\n", "\u{1}".repeat(63)).repeat(60000));
        let title = "\u{1}".repeat(1024);
        let steps = apply(&world, started(), completed_with(&world, &title, true, big));
        let text = &crate::editor_tools::proposals::tests::the_result(&steps).text;
        assert!(encoded_len(text) <= ANSWER_MAX_ENCODED, "{}", encoded_len(text));
        let answer = the_answer(&steps);
        assert_eq!(answer["proposal"], "p-1");
        assert_eq!(answer["title"], title);
    }

    #[test]
    fn an_edit_that_changes_nothing_still_names_the_command_it_skipped() {
        let world = World::new("action-empty-edit-command");
        let same = text_edit((0, 3), (0, 6), "foo");
        let steps = apply(&world, started(), completed_with(&world, "Run it", true, same));
        assert_eq!(
            the_error(&steps),
            "this action runs a command of the language server, which Eitri does not run"
        );
        let world = World::new("action-empty-edit");
        let same = text_edit((0, 3), (0, 6), "foo");
        let steps = apply(&world, started(), completed_with(&world, "Plain", false, same));
        assert_eq!(the_error(&steps), "the language server proposes no change");
    }

    #[test]
    fn a_gone_action_says_ask_again() {
        let world = World::new("action-gone");
        let steps = apply(&world, map(vec![("action_gone", true.into())]), Rmp::Nil);
        assert_eq!(
            the_error(&steps),
            "that code action is no longer known; ask `lsp_code_actions` again"
        );
        assert!(world.memory.proposal(1, "p-1").is_none());
    }

    #[test]
    fn the_proposal_names_the_actions_title() {
        let world = World::new("action-title");
        let title = "Fix \"<name>\" **literally**\u{202e}";
        the_answer(&apply(&world, started(), completed(&world, title, false)));
        assert_eq!(world.proposal("p-1").what, format!("the code action \"{title}\""));
    }

    #[test]
    fn nothing_is_written_by_either_tool() {
        let world = World::new("action-unchanged");
        the_answer(&listing(&world, vec![action("a-1", true, false, None)]));
        assert!(world.memory.proposal(1, "p-1").is_none());
        the_answer(&apply(&world, started(), completed(&world, "Fix", false)));
        assert_eq!(fs::read(world.root.join("src/a.rs")).unwrap(), b"fn foo() {}\nfoo();\n");
        assert_eq!(fs::read(world.root.join("src/b.rs")).unwrap(), b"use foo;\n");
        let proposal = world.proposal("p-1");
        let (before, after) = world.writer().texts(&proposal.files[0]).unwrap();
        assert_eq!(before, "fn foo() {}\nfoo();\n");
        assert_eq!(after, "fn bar() {}\nfoo();\n");
    }

    #[test]
    fn a_window_2_tab_is_refused() {
        let world = World::new("action-window-2");
        for name in ["lsp_code_actions", "apply_code_action"] {
            let tool = tool_named(name).expect("the tool is registered");
            for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
                let began = world.with_cx_as(mode, ProjectTrust::Trusted, EditorReach::Link, |cx| {
                    tool.begin(&json!({}), cx)
                });
                assert_eq!(began.err().unwrap().0, words::MOVE_TO_EDITOR);
            }
        }
    }

    #[test]
    fn an_n_session_is_refused() {
        let world = World::new("action-untrusted");
        for name in ["lsp_code_actions", "apply_code_action"] {
            let tool = tool_named(name).expect("the tool is registered");
            for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
                let began = world.with_cx_as(mode, ProjectTrust::Untrusted, EditorReach::Own, |cx| {
                    tool.begin(&json!({}), cx)
                });
                assert!(began.err().unwrap().0.contains("without trusting this project"));
            }
            assert!(!declared(&Switches::on_by_default(), ProjectTrust::Untrusted)
                .iter()
                .any(|row| row.name == name));
        }
    }

    #[test]
    fn a_malformed_id_is_refused() {
        let world = World::new("action-id");
        let tool = tool_named("apply_code_action").expect("apply_code_action is registered");
        for id in [
            json!(null),
            json!(1),
            json!(true),
            json!(""),
            json!("a".repeat(49)),
            json!("A-1"),
            json!("a_1"),
            json!("a/1"),
            json!("a\n1"),
            json!("é"),
        ] {
            assert!(world.with_cx(|cx| tool.begin(&json!({"id":id}), cx)).is_err(), "{id}");
        }
        assert!(world.with_cx(|cx| tool.begin(&json!({}), cx)).is_err());
        for id in ["a-0", "-", &"a".repeat(48)] {
            assert!(world.with_cx(|cx| tool.begin(&json!({"id":id}), cx)).is_ok());
        }
    }

    #[test]
    fn an_action_without_an_edit_or_command_changes_nothing() {
        let world = World::new("action-no-edit");
        let steps = apply(
            &world,
            map(vec![("no_edit", true.into()), ("has_command", false.into())]),
            Rmp::Nil,
        );
        assert_eq!(the_error(&steps), "this action changes nothing");
    }

    #[test]
    fn action_tools_are_deferred_and_can_be_switched_off() {
        let world = World::new("action-catalogue");
        let set = client_tool_set(&Switches::on_by_default(), ProjectTrust::Trusted).unwrap();
        for name in ["lsp_code_actions", "apply_code_action"] {
            let tool = tool_named(name).expect("the tool is registered");
            assert!(!tool.row().always_load);
            assert!(set.tools.iter().any(|row| row.name == name && !row.always_load));
            let off = Switches::parse(None, &[(format!("agent.editor.tool.{name}"), "off".to_owned())]).unwrap();
            assert!(!declared(&off, ProjectTrust::Trusted).iter().any(|row| row.name == name));
            assert!(world.with_cx(|cx| tool.begin(&json!({}), cx)).is_err());
        }
    }
}
