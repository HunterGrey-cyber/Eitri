//! The language server's source text about a symbol, kept as plain data.

use serde_json::{json, Value};

use super::lsp::{self, AskArgs, AskCollect, Budget, Collected, LspKind, Progress};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::step::{Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun};

pub const ROW: ToolRow = ToolRow {
    name: "hover",
    description: "Ask the language server about a symbol on a line. Returns the server's source text as data, \
                  never rendered Markdown. Text is cut at 16 KiB, with cut saying so. A file not loaded follows \
                  the file-loading card rule; an outside path follows the outside-project card rule.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute or relative to the project root." },
    "line": { "type": "integer", "minimum": 1 },
    "symbol": { "type": "string", "maxLength": 256 }
  },
  "required": ["path", "line", "symbol"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: true,
};

pub struct Hover;

impl Tool for Hover {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        let raw = args::path(args, "path")?.to_owned();
        let line = args::line(args, "line")?.ok_or_else(|| ToolError("line is required".to_owned()))?;
        let symbol = args::symbol(args, "symbol")?.to_owned();
        let template = AskArgs {
            kind: LspKind::Hover,
            line: Some(line),
            symbol: Some(symbol.clone()),
            ..Default::default()
        };
        Ok(Box::new(HoverRun {
            symbol,
            ask: AskCollect::new(template, Some(raw), Budget::new(lsp::LSP_BUDGET)),
        }))
    }
}

struct HoverRun {
    symbol: String,
    ask: AskCollect,
}

impl ToolRun for HoverRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match self.ask.step(event, cx) {
            Progress::Step(step) => step,
            Progress::Failed(result) => Step::Done(result),
            Progress::Finished(Collected::Done {
                hover, cut, missing, ..
            }) => {
                let (text, is_error) = match hover {
                    Some(text) => (
                        json!({"symbol": self.symbol, "text": text, "cut": cut}).to_string(),
                        false,
                    ),
                    None if !missing.is_empty() => ("the language server did not answer within 10 s".to_owned(), true),
                    None => (format!("the language server has nothing on `{}`", self.symbol), false),
                };
                Step::Done(ToolResult { text, is_error })
            }
            Progress::Finished(_) => Step::Done(ToolResult {
                text: "the language server's answer could not be read".to_owned(),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::editor_tools::cards::{CardDecision, CardOrigin};
    use crate::editor_tools::step::{AgentCall, EditorReach, EditorSight, Step, ToolCx, ToolResult};
    use crate::editor_tools::way::EditorCall;
    use crate::editor_tools::{
        memory::Memory,
        testing::{drive, drive_with_cards},
        tools::tool_named,
    };
    use crate::test_scratch_dir::ScratchDir;
    use agent::{setting_sources::ProjectTrust, PermissionMode};
    use rmpv::Value as Rmp;
    use serde_json::{json, Value};
    use std::path::Path;

    fn msg(value: Value) -> Rmp {
        match value {
            Value::Null => Rmp::Nil,
            Value::Bool(v) => v.into(),
            Value::Number(v) => v.as_u64().unwrap().into(),
            Value::String(v) => v.into(),
            Value::Array(v) => Rmp::Array(v.into_iter().map(msg).collect()),
            Value::Object(v) => Rmp::Map(v.into_iter().map(|(k, v)| (k.into(), msg(v))).collect()),
        }
    }
    fn with_cx(f: impl FnOnce(&ToolCx<'_>)) {
        let scratch = ScratchDir::new("editor-tools", "hover");
        let root = std::fs::canonicalize(&*scratch).unwrap();
        std::fs::write(root.join("a.py"), "foo\n").unwrap();
        let memory = Memory::default();
        f(&ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: &root,
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        });
    }
    fn result(steps: &[Step]) -> &ToolResult {
        let Some(Step::Done(result)) = steps.last() else {
            panic!("the run must finish")
        };
        result
    }
    fn answer(steps: &[Step]) -> Value {
        let result = result(steps);
        assert!(!result.is_error, "{}", result.text);
        serde_json::from_str(&result.text).unwrap()
    }
    fn reply(call: &EditorCall, done: Value) -> Rmp {
        match call {
            EditorCall::LspAsk(_) => msg(json!({"started": true, "unsupported": []})),
            EditorCall::LspCollect { .. } => msg(done),
            other => panic!("{other:?}"),
        }
    }
    fn done(parts: Value) -> Value {
        json!({"status":"done", "parts":parts, "missing":[]})
    }

    fn input(path: impl AsRef<Path>) -> Value {
        json!({"path":path.as_ref(), "line":1, "symbol":"foo"})
    }
    #[test]
    fn hover_answers_the_servers_text() {
        with_cx(|cx| {
            let run = tool_named("hover")
                .expect("hover is registered")
                .begin(&input("a.py"), cx)
                .unwrap();
            let text = "**foo** <script>server</script>\u{202e}";
            let steps = drive(run, cx, |call| {
                if let EditorCall::LspAsk(args) = call {
                    assert_eq!(args.kind, super::super::lsp::LspKind::Hover);
                }
                Ok(reply(
                    call,
                    json!({"status":"done", "parts":[], "hover":text, "cut":false, "missing":[]}),
                ))
            });
            assert_eq!(answer(&steps), json!({"symbol":"foo", "text":text, "cut":false}));
        });
    }
    /// A buffer of the project read from a file outside it: all a server says of it, and whether a line or a symbol is
    /// there, come from that file's text.
    #[test]
    fn hover_tells_nothing_of_a_buffer_read_from_outside() {
        with_cx(|cx| {
            let buffer = json!({"name": cx.root.join("a.py"), "read_from": "/elsewhere/secret.py"});
            for asked in [
                json!({"started": true, "unsupported": [], "buffer": buffer}),
                json!({"no_symbol": true, "buffer": buffer}),
                json!({"no_line": true, "line_count": 3, "buffer": buffer}),
                json!({"no_client": true, "buffer": buffer}),
            ] {
                let run = tool_named("hover").unwrap().begin(&input("a.py"), cx).unwrap();
                let steps = drive(run, cx, |call| match call {
                    EditorCall::LspAsk(_) => Ok(msg(asked.clone())),
                    other => panic!("nothing is collected: {other:?}"),
                });
                let told = result(&steps);
                assert!(told.is_error, "{told:?}");
                assert!(told.text.contains("use Read"), "{told:?}");
                assert!(!told.text.contains("secret"), "{told:?}");
            }
        });
    }
    #[test]
    fn hover_says_when_the_server_has_nothing() {
        with_cx(|cx| {
            let run = tool_named("hover").unwrap().begin(&input("a.py"), cx).unwrap();
            let steps = drive(run, cx, |call| Ok(reply(call, done(json!([])))));
            assert_eq!(result(&steps).text, "the language server has nothing on `foo`");
            assert!(!result(&steps).is_error);
        });
    }
    #[test]
    fn hover_names_a_server_that_did_not_answer() {
        with_cx(|cx| {
            let run = tool_named("hover").unwrap().begin(&input("a.py"), cx).unwrap();
            let steps = drive(run, cx, |call| {
                Ok(reply(call, json!({"status":"done", "parts":[], "missing":["pyright"]})))
            });
            assert!(result(&steps).is_error);
            assert_eq!(result(&steps).text, "the language server did not answer within 10 s");
        });
    }
    #[test]
    fn hover_marks_a_cut_text() {
        with_cx(|cx| {
            let run = tool_named("hover").unwrap().begin(&input("a.py"), cx).unwrap();
            let steps = drive(run, cx, |call| {
                Ok(reply(
                    call,
                    json!({"status":"done", "parts":[], "hover":"partial", "cut":true, "missing":[]}),
                ))
            });
            assert_eq!(answer(&steps)["cut"], true);
        });
    }
    #[test]
    fn hover_outside_is_a_card() {
        with_cx(|cx| {
            let run = tool_named("hover")
                .unwrap()
                .begin(&input(cx.root.parent().unwrap().join("outside.py")), cx)
                .unwrap();
            let steps = drive_with_cards(
                run,
                cx,
                |_| panic!("denied paths reach no editor"),
                |_| (CardDecision::Deny, CardOrigin::Key),
            );
            assert!(steps.iter().any(|s| matches!(s, Step::Ask(_))));
            assert!(result(&steps).is_error);
        });
    }
}
