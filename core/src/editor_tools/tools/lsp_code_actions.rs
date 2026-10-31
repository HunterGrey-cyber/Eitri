//! Code actions offered for one line, kept as plain server data until an edit is requested.

use serde_json::{json, Value};

use super::lsp::{self, AskArgs, AskCollect, Budget, Collected, LspKind, Progress};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::proposals::may_propose;
use crate::editor_tools::step::{
    encoded_len, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun, ANSWER_MAX_ENCODED,
};

pub const ROW: ToolRow = ToolRow {
    name: "lsp_code_actions",
    description:
        "List the language server's code actions for a line, with their ids, titles, kinds and disabled reasons. \
                  missing names a part that did not answer within 10 s. applies means an action has an edit or may resolve to one. apply_code_action computes a proposal \
                  from an id; apply_proposal applies it. Nothing is changed by listing actions, and server commands \
                  are never run. Titles and disabled reasons are plain text data. A file not loaded follows the \
                  file-loading card rule; paths outside the project are refused.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute or relative to the project root." },
    "line": { "type": "integer", "minimum": 1 }
  },
  "required": ["path", "line"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: false,
};

pub struct LspCodeActions;

impl Tool for LspCodeActions {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        may_propose(cx)?;
        let raw = args::path(args, "path")?.to_owned();
        let line = args::line(args, "line")?.ok_or_else(|| ToolError("line is required".to_owned()))?;
        let template = AskArgs {
            kind: LspKind::CodeActions,
            line: Some(line),
            ..Default::default()
        };
        Ok(Box::new(ActionsRun {
            ask: AskCollect::new(template, Some(raw), Budget::new(lsp::LSP_BUDGET)).refuse_outside(),
        }))
    }
}

struct ActionsRun {
    ask: AskCollect,
}

impl ToolRun for ActionsRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match self.ask.step(event, cx) {
            Progress::Step(step) => step,
            Progress::Failed(result) => Step::Done(result),
            Progress::Finished(Collected::Done { actions, missing, .. }) => {
                if actions.is_empty() && !missing.is_empty() {
                    return Step::Done(ToolResult {
                        text: "the language server did not answer within 10 s".to_owned(),
                        is_error: true,
                    });
                }
                let actions: Vec<_> = actions
                    .into_iter()
                    .map(|action| {
                        json!({
                            "id": action.id,
                            "title": action.title,
                            "kind": action.kind,
                            "disabled": action.disabled,
                            "applies": action.has_edit || !action.has_command,
                        })
                    })
                    .collect();
                let mut answer = json!({"actions": actions});
                if !missing.is_empty() {
                    // A server that did not answer leaves the others' actions worth listing.
                    answer["missing"] = json!(missing);
                }
                let text = answer.to_string();
                let too_large = encoded_len(&text) > ANSWER_MAX_ENCODED;
                Step::Done(ToolResult {
                    text: if too_large {
                        "the code action list is too large to show".to_owned()
                    } else {
                        text
                    },
                    is_error: too_large,
                })
            }
            Progress::Finished(_) => Step::Done(ToolResult {
                text: lsp::UNREADABLE.to_owned(),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::editor_tools::proposals::tests::{map, started, the_answer, the_error, Patient, World};
    use crate::editor_tools::step::Step;
    use crate::editor_tools::testing::drive;
    use crate::editor_tools::tools::{lsp::LspKind, tool_named};
    use crate::editor_tools::way::EditorCall;
    use rmpv::Value as Rmp;
    use serde_json::{json, Value};

    pub(crate) fn action(id: &str, edit: bool, command: bool, disabled: Option<&str>) -> Rmp {
        map(vec![
            ("id", id.into()),
            ("title", "Fix <name> **literally**\u{202e}".into()),
            ("kind", "quickfix".into()),
            ("has_edit", edit.into()),
            ("has_command", command.into()),
            ("disabled", disabled.map(Rmp::from).unwrap_or(Rmp::Nil)),
        ])
    }

    pub(crate) fn listing(world: &World, actions: Vec<Rmp>) -> Vec<Step> {
        listing_missing(world, actions, &[])
    }

    fn listing_missing(world: &World, actions: Vec<Rmp>, missing: &[&str]) -> Vec<Step> {
        world.with_cx(|cx| {
            let run = tool_named("lsp_code_actions")
                .expect("lsp_code_actions is registered")
                .begin(&json!({"path":"src/a.rs", "line":1}), cx)
                .unwrap();
            drive(Box::new(Patient(run)), cx, |call| {
                Ok(match call {
                    EditorCall::LspAsk(args) => {
                        assert_eq!(args.kind, LspKind::CodeActions);
                        assert_eq!(args.path, Some(world.root.join("src/a.rs")));
                        assert_eq!(args.line, Some(1));
                        assert!(args.symbol.is_none());
                        started()
                    }
                    EditorCall::LspCollect { .. } => map(vec![
                        ("status", "done".into()),
                        ("parts", Rmp::Array(vec![])),
                        (
                            "missing",
                            Rmp::Array(missing.iter().map(|name| Rmp::from(*name)).collect()),
                        ),
                        ("actions", Rmp::Array(actions.clone())),
                    ]),
                    other => panic!("only listing calls may run: {other:?}"),
                })
            })
        })
    }

    #[test]
    fn actions_are_listed_with_their_ids() {
        let world = World::new("actions-list");
        let actions = vec![
            action("a-1", true, false, None),
            action("a-2", false, false, None),
            action("a-3", false, true, None),
            action("a-4", true, true, None),
        ];
        let answer = the_answer(&listing(&world, actions));
        for (index, applies) in [true, true, false, true].into_iter().enumerate() {
            assert_eq!(answer["actions"][index]["id"], format!("a-{}", index + 1));
            assert_eq!(answer["actions"][index]["applies"], applies);
            assert_eq!(answer["actions"][index]["title"], "Fix <name> **literally**\u{202e}");
            assert_eq!(answer["actions"][index]["kind"], "quickfix");
            assert_eq!(answer["actions"][index]["disabled"], Value::Null);
        }
        assert!(world.memory.proposal(1, "p-1").is_none());
    }

    #[test]
    fn a_disabled_action_says_why() {
        let world = World::new("actions-disabled");
        let why = "Requires <feature> **enabled**\u{202e}";
        let answer = the_answer(&listing(&world, vec![action("a-1", true, false, Some(why))]));
        assert_eq!(answer["actions"][0]["id"], "a-1");
        assert_eq!(answer["actions"][0]["disabled"], why);
    }

    #[test]
    fn actions_that_arrived_are_listed_when_a_server_is_still_out() {
        let world = World::new("actions-partial");
        let answer = the_answer(&listing_missing(
            &world,
            vec![action("a-1", true, false, None)],
            &["code_actions"],
        ));
        assert_eq!(answer["actions"][0]["id"], "a-1");
        assert_eq!(answer["missing"], json!(["code_actions"]));
    }

    #[test]
    fn a_complete_answer_names_no_missing_part() {
        let world = World::new("actions-complete");
        let answer = the_answer(&listing(&world, vec![action("a-1", true, false, None)]));
        assert!(answer.get("missing").is_none());
    }

    #[test]
    fn no_action_and_no_answer_is_the_error() {
        let world = World::new("actions-silent");
        let steps = listing_missing(&world, vec![], &["code_actions"]);
        assert_eq!(the_error(&steps), "the language server did not answer within 10 s");
    }

    #[test]
    fn a_list_too_large_to_show_is_refused_whole() {
        let world = World::new("actions-large");
        let actions = (0..64)
            .map(|n| action(&format!("a-{n}"), true, false, Some(&"\u{1}".repeat(4000))))
            .collect();
        let steps = listing(&world, actions);
        assert_eq!(the_error(&steps), "the code action list is too large to show");
    }
}
