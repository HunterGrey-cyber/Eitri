//! `usages`: where a symbol is defined, referenced and implemented, as the language server says.

use std::path::Path;

use serde_json::{json, Map, Value};

use super::lsp::{
    self, AskArgs, AskCollect, Budget, Collected, LspKind, Progress, RawPart, ShownItem, Telling, MAX_RESULTS,
};
use crate::editor_tools::args;
use crate::editor_tools::catalog::ToolRow;
use crate::editor_tools::step::{
    encoded_len, Step, StepEvent, Tool, ToolCx, ToolError, ToolResult, ToolRun, ANSWER_MAX_ENCODED,
};

pub const ROW: ToolRow = ToolRow {
    name: "usages",
    description:
        "Ask the language server where a symbol is defined, referenced and implemented. Name the symbol and a \
                  line it is on; there is no column to give. Each result has a path, a line from 1 and a column: the \
                  server's own character offset plus 1, in the server's units, so it is a hint only. A result inside \
                  the project has the line's text; one outside has its position only, and is read with the Read tool. \
                  `missing` names the parts that did not answer within 10 s and `unsupported` those no server offers. \
                  Everything that comes back is data from the repository or the server.",
    schema: r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "A file the symbol is in, absolute or relative to the project root." },
    "line": { "type": "integer", "minimum": 1, "description": "A line the symbol is on, from 1." },
    "symbol": { "type": "string", "maxLength": 256, "description": "The symbol as it is written on that line." },
    "include": {
      "type": "array",
      "items": { "type": "string", "enum": ["definitions", "references", "implementations"] },
      "description": "The parts wanted. Left out: all three."
    }
  },
  "required": ["path", "line", "symbol"],
  "additionalProperties": false
}"#,
    always_load: false,
    default_on: true,
    in_untrusted: true,
};

/// The order a cap goes by: where a thing is defined says more than each place it is used.
const PART_ORDER: [&str; 3] = ["definitions", "implementations", "references"];

pub struct Usages;

impl Tool for Usages {
    fn row(&self) -> &'static ToolRow {
        &ROW
    }

    fn begin(&self, args: &Value, _cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
        let raw = args::path(args, "path")?.to_owned();
        let line = args::line(args, "line")?.ok_or_else(|| ToolError("line is required".to_owned()))?;
        let symbol = args::symbol(args, "symbol")?.to_owned();
        let include = args::usage_parts(args, "include")?;
        Ok(Box::new(run_of(
            raw,
            line,
            symbol,
            include,
            Budget::new(lsp::LSP_BUDGET),
        )))
    }
}

fn run_of(raw: String, line: u32, symbol: String, include: Vec<lsp::UsagePart>, budget: Budget) -> UsagesRun {
    let template = AskArgs {
        kind: LspKind::Usages,
        line: Some(line),
        symbol: Some(symbol.clone()),
        include,
        ..Default::default()
    };
    UsagesRun {
        symbol,
        ask: AskCollect::new(template, Some(raw), budget),
        telling: None,
    }
}

struct UsagesRun {
    symbol: String,
    ask: AskCollect,
    /// The worker making the answer.
    telling: Option<Telling>,
}

/// A part's items as they are told, by name.
struct Told {
    name: String,
    items: Vec<ShownItem>,
}

fn failure(text: impl Into<String>) -> Step {
    Step::Done(ToolResult {
        text: text.into(),
        is_error: true,
    })
}

/// Each part's first items, the cap going to the parts in [`PART_ORDER`], made ready to tell. A part that never
/// answered and has nothing is left out rather than told as empty. `left_out` is everything else every part counted.
fn told_parts(root: &Path, home: Option<&Path>, mut parts: Vec<RawPart>, missing: &[String]) -> (Vec<Told>, u64) {
    let counted: u64 = parts.iter().map(|part| u64::from(part.total)).sum();
    let mut room = MAX_RESULTS;
    let mut told = Vec::new();
    for name in PART_ORDER {
        let Some(at) = parts.iter().position(|part| part.name == name) else {
            continue;
        };
        let mut part = parts.swap_remove(at);
        if part.items.is_empty() && missing.contains(&part.name) {
            continue;
        }
        part.items.truncate(room);
        room -= part.items.len();
        told.push(Told {
            name: part.name,
            items: lsp::shown(root, home, part.items),
        });
    }
    let kept: u64 = told.iter().map(|part| part.items.len() as u64).sum();
    (told, counted.saturating_sub(kept))
}

/// The answer with the first `kept` items, in the order of `told`.
fn answer_text(
    symbol: &str,
    told: &[Told],
    missing: &[String],
    unsupported: &[String],
    left_out: u64,
    kept: usize,
) -> String {
    let mut answer = Map::new();
    answer.insert("symbol".to_owned(), json!(symbol));
    let mut room = kept;
    for part in told {
        let here = part.items.len().min(room);
        room -= here;
        answer.insert(part.name.clone(), json!(&part.items[..here]));
    }
    let all: usize = told.iter().map(|part| part.items.len()).sum();
    answer.insert("missing".to_owned(), json!(missing));
    answer.insert("unsupported".to_owned(), json!(unsupported));
    answer.insert("left_out".to_owned(), json!(left_out + (all - kept) as u64));
    Value::Object(answer).to_string()
}

/// The whole answer, off the GTK thread: it reads files, and its fitting writes the answer out several times.
fn the_answer(
    root: &Path,
    home: Option<&Path>,
    symbol: &str,
    parts: Vec<RawPart>,
    missing: &[String],
    unsupported: &[String],
) -> ToolResult {
    let (told, left_out) = told_parts(root, home, parts, missing);
    let ready = |kept: usize| answer_text(symbol, &told, missing, unsupported, left_out, kept);
    // Whole items come off the end until the answer fits. What fits only shrinks as items are added, so the largest
    // count that does is found by halving.
    let all: usize = told.iter().map(|part| part.items.len()).sum();
    let mut kept = all;
    if encoded_len(&ready(all)) > ANSWER_MAX_ENCODED {
        let (mut fits, mut over) = (0, all);
        while over - fits > 1 {
            let middle = fits + (over - fits) / 2;
            if encoded_len(&ready(middle)) <= ANSWER_MAX_ENCODED {
                fits = middle;
            } else {
                over = middle;
            }
        }
        kept = fits;
    }
    ToolResult {
        text: ready(kept),
        is_error: false,
    }
}

/// Nothing came from any part: every one of them is still out, and with no result.
fn nothing_arrived(parts: &[RawPart], missing: &[String]) -> bool {
    !missing.is_empty() && parts.iter().all(|part| part.total == 0 && missing.contains(&part.name))
}

impl UsagesRun {
    fn collected(&mut self, collected: Collected, cx: &ToolCx<'_>) -> Step {
        let Collected::Done { parts, missing, .. } = collected else {
            return failure(lsp::UNREADABLE);
        };
        if nothing_arrived(&parts, &missing) {
            return failure("the language server did not answer within 10 s");
        }
        let root = cx.root.to_path_buf();
        let home = cx.home.map(Path::to_path_buf);
        let symbol = self.symbol.clone();
        let unsupported = self.ask.unsupported().to_vec();
        self.telling
            .insert(Telling::start(move || {
                the_answer(&root, home.as_deref(), &symbol, parts, &missing, &unsupported)
            }))
            .step()
    }
}

impl ToolRun for UsagesRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        if let Some(telling) = &self.telling {
            return telling.step();
        }
        match self.ask.step(event, cx) {
            Progress::Step(step) => step,
            Progress::Failed(result) => Step::Done(result),
            Progress::Finished(collected) => self.collected(collected, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::lsp::{RawItem, ATTACH_WAIT, COLLECT_MARGIN, LSP_BUDGET};
    use super::*;
    use crate::editor_tools::cards::{CardDecision, CardOrigin};
    use crate::editor_tools::catalog::{client_tool_set, declared};
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::stamp::EDITOR_BUDGET;
    use crate::editor_tools::step::{AgentCall, EditorFailure, EditorReach, EditorSight};
    use crate::editor_tools::switches::Switches;
    use crate::editor_tools::testing::{drive, drive_with_cards};
    use crate::editor_tools::way::EditorCall;
    use crate::editor_tools::words;
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use rmpv::Value as Rmp;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use PermissionMode::{Auto, Bypass, Manual};
    use ProjectTrust::{Trusted, Untrusted};

    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-usages", label);
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(root.join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
        fs::write(beside.join("b.rs"), "outside line\n").unwrap();
        Project {
            _scratch: scratch,
            root,
            beside,
        }
    }

    struct Setting {
        mode: PermissionMode,
        trust: ProjectTrust,
        reach: EditorReach,
    }

    fn setting(mode: PermissionMode, trust: ProjectTrust) -> Setting {
        Setting {
            mode,
            trust,
            reach: EditorReach::Own,
        }
    }

    fn with_cx<R>(root: &Path, setting: &Setting, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let _clock = crate::editor_tools::testing::clock::Clock::new();
        let memory = Memory::default();
        let cx = ToolCx {
            mode: setting.mode,
            trust: setting.trust,
            root,
            home: None,
            reach: setting.reach,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        f(&cx)
    }

    fn run(raw: &str, budget: Duration) -> Box<UsagesRun> {
        Box::new(run_of(
            raw.to_owned(),
            2,
            "foo".to_owned(),
            Vec::new(),
            Budget::new(budget),
        ))
    }

    fn map(entries: Vec<(&str, Rmp)>) -> Rmp {
        Rmp::Map(entries.into_iter().map(|(k, v)| (Rmp::from(k), v)).collect())
    }

    fn started() -> Rmp {
        map(vec![
            ("started", Rmp::from(true)),
            ("unsupported", Rmp::Array(Vec::new())),
        ])
    }

    fn needs_load() -> Rmp {
        map(vec![("needs_load", Rmp::from(true))])
    }

    fn no_client(loaded_now: bool) -> Rmp {
        map(vec![
            ("no_client", Rmp::from(true)),
            ("loaded_now", Rmp::from(loaded_now)),
        ])
    }

    fn pending() -> Rmp {
        map(vec![("status", Rmp::from("pending"))])
    }

    fn gone() -> Rmp {
        map(vec![("status", Rmp::from("gone"))])
    }

    fn item(path: &Path, line: u32) -> Rmp {
        map(vec![
            ("path", Rmp::Binary(path.as_os_str().as_bytes().to_vec())),
            ("line", Rmp::from(line)),
            ("character", Rmp::from(4)),
        ])
    }

    fn part(name: &str, items: Vec<Rmp>, total: u32) -> Rmp {
        map(vec![
            ("name", Rmp::from(name)),
            ("items", Rmp::Array(items)),
            ("total", Rmp::from(total)),
        ])
    }

    fn done(parts: Vec<Rmp>, missing: &[&str]) -> Rmp {
        map(vec![
            ("status", Rmp::from("done")),
            ("parts", Rmp::Array(parts)),
            (
                "missing",
                Rmp::Array(missing.iter().map(|name| Rmp::from(*name)).collect()),
            ),
        ])
    }

    fn one_definition(root: &Path) -> Rmp {
        done(vec![part("definitions", vec![item(&root.join("src/a.rs"), 2)], 1)], &[])
    }

    /// The editor of the common run: the ask starts, its first collect answers.
    fn answers_at_once(root: &Path) -> impl FnMut(&EditorCall) -> Result<Rmp, EditorFailure> + '_ {
        move |call| {
            Ok(match call {
                EditorCall::LspAsk(_) => started(),
                EditorCall::LspCollect { .. } => one_definition(root),
                other => panic!("{other:?}"),
            })
        }
    }

    fn the_result(steps: &[Step]) -> &ToolResult {
        match steps.last() {
            Some(Step::Done(result)) => result,
            other => panic!("the run did not end: {other:?}"),
        }
    }

    fn the_error(steps: &[Step]) -> &str {
        let result = the_result(steps);
        assert!(result.is_error, "{result:?}");
        &result.text
    }

    fn the_answer_of(steps: &[Step]) -> serde_json::Value {
        let result = the_result(steps);
        assert!(!result.is_error, "{result:?}");
        serde_json::from_str(&result.text).unwrap()
    }

    fn asks(steps: &[Step]) -> Vec<&AskArgs> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Call {
                    call: EditorCall::LspAsk(args),
                    ..
                } => Some(args),
                _ => None,
            })
            .collect()
    }

    fn cards(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Ask(card) => Some(card.title.as_str()),
                _ => None,
            })
            .collect()
    }

    fn drive_allowing(
        run: Box<UsagesRun>,
        cx: &ToolCx<'_>,
        answers: impl FnMut(&EditorCall) -> Result<Rmp, EditorFailure>,
        decision: CardDecision,
    ) -> Vec<Step> {
        drive_with_cards(run, cx, answers, move |_| (decision, CardOrigin::Key))
    }

    #[test]
    fn inside_and_loaded_runs_with_no_card_in_every_mode() {
        let project = project("loaded");
        for mode in [Manual, Auto, Bypass] {
            for trust in [Trusted, Untrusted] {
                let setting = setting(mode, trust);
                let steps = with_cx(&project.root, &setting, |cx| {
                    drive(run("src/a.rs", LSP_BUDGET), cx, answers_at_once(&project.root))
                });
                let answer = the_answer_of(&steps);
                assert_eq!(answer["symbol"], "foo");
                assert_eq!(
                    answer["definitions"],
                    json!([{"path": "src/a.rs", "line": 2, "column": 5, "text": "two"}])
                );
            }
        }
    }

    #[test]
    fn an_ask_says_what_was_asked() {
        let project = project("ask");
        let setting = setting(Bypass, Trusted);
        let mut run = run_of(
            "src/a.rs".to_owned(),
            2,
            "foo".to_owned(),
            vec![lsp::UsagePart::References],
            Budget::new(LSP_BUDGET),
        );
        let step = with_cx(&project.root, &setting, |cx| run.step(StepEvent::Start, cx));
        let Step::Call {
            call: EditorCall::LspAsk(args),
            budget,
        } = step
        else {
            panic!("{step:?}");
        };
        assert_eq!(budget, EDITOR_BUDGET);
        assert_eq!(args.id, "call-1");
        assert_eq!(args.root, project.root);
        assert_eq!(args.path, Some(project.root.join("src/a.rs")));
        assert_eq!(args.line, Some(2));
        assert_eq!(args.symbol.as_deref(), Some("foo"));
        assert_eq!(args.include, [lsp::UsagePart::References]);
        let whole = (LSP_BUDGET + COLLECT_MARGIN).as_millis() as u32;
        assert!(
            args.budget_ms <= whole && args.budget_ms > whole - 1000,
            "{}",
            args.budget_ms
        );
    }

    #[test]
    fn an_unloaded_file_is_a_card_in_manual_and_in_an_n_session() {
        let project = project("unloaded");
        let file = project.root.join("src/a.rs");
        for setting in [setting(Manual, Trusted), setting(Auto, Untrusted)] {
            let editor = |call: &EditorCall| {
                Ok(match call {
                    EditorCall::LspAsk(args) if !args.load => needs_load(),
                    EditorCall::LspAsk(_) => started(),
                    EditorCall::LspCollect { .. } => one_definition(&project.root),
                    other => panic!("{other:?}"),
                })
            };
            let steps = with_cx(&project.root, &setting, |cx| {
                drive_allowing(run("src/a.rs", LSP_BUDGET), cx, editor, CardDecision::Allow)
            });
            assert_eq!(cards(&steps), [format!("Load {} in your editor?", file.display())]);
            let asked = asks(&steps);
            assert_eq!(
                asked.iter().map(|args| args.load).collect::<Vec<_>>(),
                [false, true],
                "the first ask loads nothing"
            );
            assert_ne!(asked[0].id, asked[1].id, "an id is for one ask");
            the_answer_of(&steps);
        }
    }

    #[test]
    fn an_auto_y_tab_loads_without_a_card() {
        let project = project("auto-y");
        let steps = with_cx(&project.root, &setting(Auto, Trusted), |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, answers_at_once(&project.root))
        });
        assert!(asks(&steps)[0].load);
    }

    #[test]
    fn bypass_loads_without_a_card() {
        let project = project("bypass");
        for trust in [Trusted, Untrusted] {
            let steps = with_cx(&project.root, &setting(Bypass, trust), |cx| {
                drive(run("src/a.rs", LSP_BUDGET), cx, answers_at_once(&project.root))
            });
            assert!(asks(&steps)[0].load, "{trust:?}");
        }
    }

    #[test]
    fn outside_is_a_card_in_manual_and_auto_and_allowed_in_bypass() {
        let project = project("outside");
        let outside = project.beside.join("b.rs");
        let raw = outside.to_str().unwrap();
        for mode in [Manual, Auto] {
            let steps = with_cx(&project.root, &setting(mode, Trusted), |cx| {
                drive_allowing(
                    run(raw, LSP_BUDGET),
                    cx,
                    answers_at_once(&project.root),
                    CardDecision::Allow,
                )
            });
            assert_eq!(
                cards(&steps),
                [format!("Ask the language server about {}?", outside.display())]
            );
            let asked = asks(&steps);
            assert_eq!(asked[0].path.as_deref(), Some(outside.as_path()));
            assert!(asked[0].load);
        }
        let steps = with_cx(&project.root, &setting(Bypass, Untrusted), |cx| {
            drive(run(raw, LSP_BUDGET), cx, answers_at_once(&project.root))
        });
        assert_eq!(asks(&steps)[0].path.as_deref(), Some(outside.as_path()));
    }

    #[test]
    fn an_outside_card_says_what_it_risks() {
        let project = project("outside-words");
        let outside = project.beside.join("b.rs");
        let mut run = run(outside.to_str().unwrap(), LSP_BUDGET);
        let step = with_cx(&project.root, &setting(Manual, Trusted), |cx| {
            run.step(StepEvent::Start, cx)
        });
        let Step::Ask(card) = step else {
            panic!("{step:?}");
        };
        assert_eq!(
            card.body,
            [crate::editor_tools::cards::CardLine::Plain(
                "It is outside the project; opening it can start language servers and filetype plugins there"
                    .to_owned()
            )]
        );
    }

    #[test]
    fn an_outside_card_approves_only_the_place_it_named() {
        let project = project("outside-moved");
        let link = project.root.join("link.rs");
        std::os::unix::fs::symlink(project.beside.join("b.rs"), &link).unwrap();
        let other = project.beside.join("other.rs");
        fs::write(&other, "x").unwrap();
        let setting = setting(Manual, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive_with_cards(
                run("link.rs", LSP_BUDGET),
                cx,
                |call| panic!("nothing reaches the editor: {call:?}"),
                |_| {
                    fs::remove_file(&link).unwrap();
                    std::os::unix::fs::symlink(&other, &link).unwrap();
                    (CardDecision::Allow, CardOrigin::Key)
                },
            )
        });
        assert!(the_error(&steps).contains("now leads somewhere other"), "{steps:?}");
    }

    #[test]
    fn a_load_card_checks_the_file_again() {
        let project = project("load-moved");
        let file = project.root.join("src/a.rs");
        let setting = setting(Manual, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive_with_cards(
                run("src/a.rs", LSP_BUDGET),
                cx,
                |_| Ok(needs_load()),
                |_| {
                    fs::remove_file(&file).unwrap();
                    let fifo = CString::new(file.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
                    (CardDecision::Allow, CardOrigin::Key)
                },
            )
        });
        assert_eq!(asks(&steps).len(), 1, "a FIFO would hold the editor");
        assert!(the_error(&steps).contains("not a regular file"), "{steps:?}");
    }

    #[test]
    fn a_deny_runs_nothing() {
        let project = project("deny");
        let outside = project.beside.join("b.rs");
        let setting = setting(Manual, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive_allowing(
                run(outside.to_str().unwrap(), LSP_BUDGET),
                cx,
                |call| panic!("nothing reaches the editor: {call:?}"),
                CardDecision::Deny,
            )
        });
        assert_eq!(the_error(&steps), words::NOT_ALLOWED);

        let steps = with_cx(&project.root, &setting, |cx| {
            drive_allowing(
                run("src/a.rs", LSP_BUDGET),
                cx,
                |_| Ok(needs_load()),
                CardDecision::Deny,
            )
        });
        assert_eq!(the_error(&steps), words::NOT_ALLOWED);
        assert_eq!(asks(&steps).len(), 1);
    }

    #[test]
    fn a_missing_file_asks_without_loading() {
        let project = project("missing");
        for mode in [Manual, Auto, Bypass] {
            let steps = with_cx(&project.root, &setting(mode, Trusted), |cx| {
                drive(run("src/new.rs", LSP_BUDGET), cx, answers_at_once(&project.root))
            });
            let asked = asks(&steps);
            assert_eq!(asked[0].path, Some(project.root.join("src/new.rs")));
            assert!(!asked[0].load, "{mode:?}");
            the_answer_of(&steps);
        }
    }

    #[test]
    fn a_missing_file_with_no_buffer_says_no_such_file_with_no_card() {
        let project = project("missing-none");
        for mode in [Manual, Auto, Bypass] {
            let steps = with_cx(&project.root, &setting(mode, Trusted), |cx| {
                drive(run("src/new.rs", LSP_BUDGET), cx, |_| Ok(needs_load()))
            });
            assert_eq!(
                the_error(&steps),
                format!("no such file: {}", project.root.join("src/new.rs").display())
            );
        }
    }

    #[test]
    fn a_window_2_tab_outside_is_refused_with_no_card() {
        let project = project("link");
        let outside = project.beside.join("b.rs");
        for mode in [Manual, Bypass] {
            let mut setting = setting(mode, Trusted);
            setting.reach = EditorReach::Link;
            let steps = with_cx(&project.root, &setting, |cx| {
                drive(run(outside.to_str().unwrap(), LSP_BUDGET), cx, |call| {
                    panic!("nothing reaches the editor: {call:?}")
                })
            });
            assert_eq!(the_error(&steps), words::LINK);
        }
    }

    #[test]
    fn a_fifo_never_reaches_the_editor() {
        let project = project("fifo");
        let fifo = project.root.join("pipe");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        for mode in [Manual, Auto, Bypass] {
            let steps = with_cx(&project.root, &setting(mode, Trusted), |cx| {
                drive(run("pipe", LSP_BUDGET), cx, |call| {
                    panic!("nothing reaches the editor: {call:?}")
                })
            });
            assert!(the_error(&steps).contains("not a regular file"), "{mode:?}");
        }
    }

    #[test]
    fn a_control_character_in_the_path_is_refused() {
        let project = project("control");
        let steps = with_cx(&project.root, &setting(Bypass, Trusted), |cx| {
            drive(run("src/a\n.rs", LSP_BUDGET), cx, |call| {
                panic!("nothing reaches the editor: {call:?}")
            })
        });
        assert!(the_error(&steps).contains("control character"), "{steps:?}");
    }

    fn refusal(args: serde_json::Value) -> String {
        let project = project("arguments");
        with_cx(&project.root, &setting(Bypass, Trusted), |cx| {
            Usages.begin(&args, cx).map(|_| ()).unwrap_err().0
        })
    }

    #[test]
    fn a_symbol_with_a_newline_is_refused() {
        assert!(
            refusal(json!({"path": "a", "line": 1, "symbol": "a\nb"})).contains("control character"),
            "a symbol reaches a chunk that searches one line"
        );
        assert_eq!(refusal(json!({"path": "a", "line": 1})), "symbol is required");
        assert_eq!(
            refusal(json!({"path": "a", "line": 1, "symbol": ""})),
            "symbol may not be empty"
        );
        assert_eq!(
            refusal(json!({"path": "a", "line": 1, "symbol": 3})),
            "symbol must be a string"
        );
        assert!(refusal(json!({"path": "a", "line": 1, "symbol": "x".repeat(257)})).contains("over 256"));
    }

    #[test]
    fn a_line_of_the_wrong_type_is_refused() {
        for bad in [json!("12"), json!(1.5), json!(0), json!(-1)] {
            let refused = refusal(json!({"path": "a", "line": bad, "symbol": "x"}));
            assert!(refused.starts_with("line "), "{refused}");
        }
        assert_eq!(refusal(json!({"path": "a", "symbol": "x"})), "line is required");
    }

    #[test]
    fn an_unknown_include_is_refused() {
        for bad in [json!(["callers"]), json!("references"), json!([1])] {
            let refused = refusal(json!({"path": "a", "line": 1, "symbol": "x", "include": bad}));
            assert!(refused.starts_with("include must be"), "{refused}");
        }
    }

    #[test]
    fn a_just_loaded_file_is_asked_again_until_a_client_attaches() {
        let project = project("attach");
        let setting = setting(Bypass, Trusted);
        let asked = Cell::new(0);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |call| {
                Ok(match call {
                    EditorCall::LspAsk(_) => {
                        asked.set(asked.get() + 1);
                        match asked.get() {
                            1 => no_client(true),
                            // To this ask the file is loaded, so the chunk no longer says it just was.
                            2 => no_client(false),
                            _ => started(),
                        }
                    }
                    _ => one_definition(&project.root),
                })
            })
        });
        assert_eq!(
            asks(&steps).iter().map(|args| args.load).collect::<Vec<_>>(),
            [true, false, false],
            "only the first ask may load: nothing was checked since"
        );
        the_answer_of(&steps);
        assert!(steps
            .iter()
            .any(|step| matches!(step, Step::PollAfter(wait) if *wait == Duration::from_millis(200))));
    }

    #[test]
    fn a_file_closed_while_a_client_attaches_ends_the_run_with_no_card() {
        let project = project("attach-closed");
        let setting = setting(Bypass, Trusted);
        let asked = Cell::new(0);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |_| {
                asked.set(asked.get() + 1);
                Ok(if asked.get() == 1 {
                    no_client(true)
                } else {
                    needs_load()
                })
            })
        });
        assert_eq!(
            the_error(&steps),
            format!(
                "{} was closed in the editor while its language server attached; nothing was read",
                project.root.join("src/a.rs").display()
            )
        );
    }

    #[test]
    fn no_client_after_a_load_gives_up_after_5_s() {
        assert_eq!(ATTACH_WAIT, Duration::from_secs(5), "the sentence says 5 s");
        let project = project("attach-gives-up");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |_| Ok(no_client(true)))
        });
        let polls: Vec<_> = steps
            .iter()
            .filter_map(|step| match step {
                Step::PollAfter(wait) => Some(*wait),
                _ => None,
            })
            .collect();
        assert_eq!(polls, vec![Duration::from_millis(200); 25]);
        assert_eq!(
            the_error(&steps),
            format!(
                "no language server attached to {} within 5 s",
                project.root.join("src/a.rs").display()
            )
        );
    }

    #[test]
    fn no_client_on_a_loaded_buffer_answers_at_once() {
        let project = project("no-client");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |_| Ok(no_client(false)))
        });
        assert_eq!(
            the_error(&steps),
            format!(
                "no language server is attached to {}",
                project.root.join("src/a.rs").display()
            )
        );
        assert_eq!(asks(&steps).len(), 1);
    }

    /// An editor whose collects stay pending until the last, which answers `last`.
    fn hung_until_last(last: Rmp) -> impl FnMut(&EditorCall) -> Result<Rmp, EditorFailure> {
        move |call| {
            Ok(match call {
                EditorCall::LspAsk(_) => started(),
                EditorCall::LspCollect { last: true, .. } => last.clone(),
                _ => {
                    crate::editor_tools::testing::clock::advance(Duration::from_millis(2));
                    pending()
                }
            })
        }
    }

    /// Whether each collect was the last, and its budget, oldest first.
    fn collects(steps: &[Step]) -> Vec<(bool, Duration)> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Call {
                    call: EditorCall::LspCollect { last, .. },
                    budget,
                } => Some((*last, *budget)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_budget_ends_with_what_arrived_and_names_what_is_missing() {
        let project = project("budget");
        let setting = setting(Bypass, Trusted);
        let last = done(
            vec![
                part("definitions", vec![item(&project.root.join("src/a.rs"), 1)], 1),
                part("references", vec![], 0),
            ],
            &["references"],
        );
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", Duration::from_secs(1)), cx, hung_until_last(last))
        });
        let answer = the_answer_of(&steps);
        assert_eq!(answer["definitions"][0]["line"], 1);
        assert_eq!(answer["missing"], json!(["references"]));
        assert!(
            answer.get("references").is_none(),
            "a part that never answered is not an empty one: {answer}"
        );
        let collects = collects(&steps);
        assert_eq!(collects.last().map(|call| call.0), Some(true));
        assert_eq!(collects.iter().filter(|call| call.0).count(), 1, "one last collect");
    }

    #[test]
    fn the_last_collect_has_its_own_budget() {
        let project = project("last-budget");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(
                run("src/a.rs", Duration::from_secs(1)),
                cx,
                hung_until_last(one_definition(&project.root)),
            )
        });
        let collects = collects(&steps);
        assert_eq!(collects.last(), Some(&(true, COLLECT_MARGIN)));
        for (last, budget) in &collects[..collects.len() - 1] {
            assert!(!last);
            assert!(*budget <= EDITOR_BUDGET, "{budget:?}");
        }
    }

    #[test]
    fn nothing_arrived_is_an_error() {
        let project = project("nothing");
        let setting = setting(Bypass, Trusted);
        let last = done(
            vec![part("definitions", vec![], 0), part("references", vec![], 0)],
            &["definitions", "references"],
        );
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", Duration::from_secs(1)), cx, hung_until_last(last))
        });
        assert_eq!(the_error(&steps), "the language server did not answer within 10 s");
    }

    #[test]
    fn a_part_that_answered_with_nothing_has_arrived() {
        let project = project("empty-part");
        let setting = setting(Bypass, Trusted);
        let last = done(
            vec![part("definitions", vec![], 0), part("references", vec![], 0)],
            &["references"],
        );
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", Duration::from_secs(1)), cx, hung_until_last(last))
        });
        let answer = the_answer_of(&steps);
        assert_eq!(answer["definitions"], json!([]));
        assert_eq!(answer["missing"], json!(["references"]));
    }

    #[test]
    fn the_budget_does_not_run_while_a_card_waits() {
        let project = project("card-budget");
        let setting = setting(Manual, Trusted);
        let budget = Duration::from_secs(1);
        let answered = Cell::new(0);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive_with_cards(
                run("src/a.rs", budget),
                cx,
                |call| {
                    Ok(match call {
                        EditorCall::LspAsk(args) if !args.load => needs_load(),
                        EditorCall::LspAsk(_) => started(),
                        _ => {
                            answered.set(answered.get() + 1);
                            match answered.get() {
                                1 => pending(),
                                _ => one_definition(&project.root),
                            }
                        }
                    })
                },
                |_| {
                    crate::editor_tools::testing::clock::advance(budget + Duration::from_millis(20));
                    (CardDecision::Allow, CardOrigin::Key)
                },
            )
        });
        let asked = asks(&steps);
        let life = asked[1].budget_ms;
        assert!(
            life > COLLECT_MARGIN.as_millis() as u32,
            "what was left after the card goes into the entry's life: {life}"
        );
        assert!(
            collects(&steps).iter().all(|(last, _)| !last),
            "the budget had not ended"
        );
    }

    #[test]
    fn a_collect_the_editor_did_not_answer_polls_again() {
        let project = project("collect-late");
        let setting = setting(Bypass, Trusted);
        let answered = Cell::new(0);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |call| match call {
                EditorCall::LspAsk(_) => Ok(started()),
                _ => {
                    answered.set(answered.get() + 1);
                    if answered.get() == 1 {
                        Err(EditorFailure::NotAnswered)
                    } else {
                        Ok(one_definition(&project.root))
                    }
                }
            })
        });
        the_answer_of(&steps);
        assert_eq!(answered.get(), 2);
    }

    #[test]
    fn gone_says_the_editor_restarted() {
        let project = project("gone");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |call| {
                Ok(match call {
                    EditorCall::LspAsk(_) => started(),
                    _ => gone(),
                })
            })
        });
        assert_eq!(the_error(&steps), "the editor restarted; nothing was read");
    }

    #[test]
    fn an_answer_of_the_editor_becomes_a_sentence() {
        let project = project("sentences");
        let own = setting(Bypass, Trusted);
        let mut link = setting(Bypass, Trusted);
        link.reach = EditorReach::Link;
        let file = project.root.join("src/a.rs");
        let ending = |setting: &Setting, answer: Result<Rmp, EditorFailure>| {
            let answer = RefCell::new(Some(answer));
            with_cx(&project.root, setting, |cx| {
                let steps = drive(run("src/a.rs", LSP_BUDGET), cx, |_| {
                    answer.borrow_mut().take().expect("one ask")
                });
                the_error(&steps).to_owned()
            })
        };
        assert_eq!(ending(&own, Err(EditorFailure::NotAnswered)), words::NOT_READ);
        assert_eq!(
            ending(&link, Err(EditorFailure::Unavailable("gone".to_owned()))),
            words::WINDOW_CLOSED
        );
        assert_eq!(ending(&own, Err(EditorFailure::Unavailable("gone".to_owned()))), "gone");
        assert_eq!(ending(&own, Err(EditorFailure::Refused("no".to_owned()))), "no");
        assert_eq!(
            ending(&own, Ok(map(vec![("swap", Rmp::from(true))]))),
            format!(
                "{} has a swap file (another editor, or a crash); it was not opened",
                file.display()
            )
        );
        assert_eq!(
            ending(&own, Ok(map(vec![("no_symbol", Rmp::from(true))]))),
            format!("`foo` is not on line 2 of {}", file.display())
        );
        assert_eq!(
            ending(
                &own,
                Ok(map(vec![("no_line", Rmp::from(true)), ("line_count", Rmp::from(1))]))
            ),
            format!("line 2 is past the end of {}, which has 1 line", file.display())
        );
        assert_eq!(
            ending(
                &own,
                Ok(map(vec![("no_line", Rmp::from(true)), ("line_count", Rmp::from(0))]))
            ),
            format!("line 2 is past the end of {}, which has 0 lines", file.display())
        );
        assert_eq!(ending(&own, Ok(map(vec![("refused", Rmp::from("why"))]))), "why");
        assert!(ending(&own, Ok(Rmp::Nil)).contains("could not be read"));
    }

    #[test]
    fn an_editor_held_through_the_last_collect_read_nothing() {
        let project = project("last-late");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", Duration::from_secs(1)), cx, |call| match call {
                EditorCall::LspAsk(_) => Ok(started()),
                EditorCall::LspCollect { .. } => {
                    crate::editor_tools::testing::clock::advance(Duration::from_secs(2));
                    Err(EditorFailure::NotAnswered)
                }
                other => panic!("{other:?}"),
            })
        });
        assert_eq!(the_error(&steps), words::NOT_READ);
    }

    #[test]
    fn unsupported_parts_are_named() {
        let project = project("unsupported");
        let setting = setting(Bypass, Trusted);
        let steps = with_cx(&project.root, &setting, |cx| {
            drive(run("src/a.rs", LSP_BUDGET), cx, |call| {
                Ok(match call {
                    EditorCall::LspAsk(_) => map(vec![
                        ("started", Rmp::from(true)),
                        ("unsupported", Rmp::Array(vec![Rmp::from("implementations")])),
                    ]),
                    _ => one_definition(&project.root),
                })
            })
        });
        let answer = the_answer_of(&steps);
        assert_eq!(answer["unsupported"], json!(["implementations"]));
        assert!(answer.get("implementations").is_none(), "{answer}");
    }

    fn raw(path: impl AsRef<Path>, line: u32) -> RawItem {
        RawItem {
            path: path.as_ref().to_string_lossy().into_owned(),
            line,
            character: 0,
            encoding: crate::editor_tools::text_edits::Encoding::Utf16,
            text: None,
            text_from: None,
            read_from: None,
            name: None,
            kind: None,
            container: None,
        }
    }

    #[test]
    fn a_line_from_a_buffer_outside_the_project_is_not_told() {
        let project = project("text-from");
        let inside = project.root.join("src/a.rs");
        let outside = project.beside.join("b.rs");
        let item = |from: &Path| RawItem {
            text: Some("the buffer's line".to_owned()),
            text_from: Some(from.to_path_buf()),
            read_from: Some(from.to_path_buf()),
            ..raw(&inside, 1)
        };
        let shown = lsp::shown(&project.root, None, vec![item(&outside), item(&inside)]);
        assert_eq!(shown[0].path, "src/a.rs", "the item stays, judged by its own path");
        assert_eq!(shown[0].text, None);
        assert!(!format!("{shown:?}").contains(outside.to_str().unwrap()));
        assert_eq!(shown[1].text.as_deref(), Some("the buffer's line"));
        let term = lsp::shown(&project.root, None, vec![item(Path::new("term://bash"))]);
        assert_eq!(
            term[0].text, None,
            "a name that is not absolute is no file of the project"
        );
    }

    #[test]
    fn an_item_of_a_buffer_read_from_outside_keeps_only_its_place() {
        let project = project("record-outside");
        let inside = project.root.join("src/a.rs");
        let outside = project.beside.join("b.rs");
        let item = |record: Option<&Path>| RawItem {
            text: Some("the buffer's line".to_owned()),
            text_from: Some(inside.clone()),
            read_from: record.map(Path::to_path_buf),
            name: Some("name".to_owned()),
            kind: Some("Function".to_owned()),
            container: Some("container".to_owned()),
            ..raw(&inside, 1)
        };
        let shown = lsp::shown(
            &project.root,
            None,
            vec![item(Some(&outside)), item(None), item(Some(&inside))],
        );
        for (index, kept) in shown[..2].iter().enumerate() {
            assert_eq!(kept.path, "src/a.rs", "{index}");
            assert_eq!(
                (&kept.text, &kept.name, &kept.kind, &kept.container),
                (&None, &None, &None, &None),
                "nothing taken from the buffer: {index}"
            );
        }
        assert_eq!(shown[2].text.as_deref(), Some("the buffer's line"));
        assert_eq!(shown[2].name.as_deref(), Some("name"));
    }

    #[test]
    fn an_item_outside_keeps_only_its_path_and_position() {
        let project = project("outside-item");
        let item = RawItem {
            character: 6,
            text: Some("secret line".to_owned()),
            text_from: Some(PathBuf::from("/usr/lib/x.rs")),
            name: Some("name".to_owned()),
            kind: Some("Function".to_owned()),
            container: Some("container".to_owned()),
            ..raw("/usr/lib/x.rs", 9)
        };
        let shown = lsp::shown(&project.root, None, vec![item]);
        assert_eq!(
            shown,
            [ShownItem {
                path: "/usr/lib/x.rs".to_owned(),
                line: 9,
                column: 7,
                text: None,
                name: None,
                kind: None,
                container: None,
            }]
        );
    }

    #[test]
    fn a_link_leaving_the_root_is_outside() {
        let project = project("link-out");
        let link = project.root.join("leaves.rs");
        std::os::unix::fs::symlink(project.beside.join("b.rs"), &link).unwrap();
        let shown = lsp::shown(&project.root, None, vec![raw(&link, 1)]);
        assert_eq!(shown[0].path, link.to_string_lossy());
        assert_eq!(shown[0].text, None, "the file's line stays out");
    }

    #[test]
    fn a_uri_is_outside_as_it_stands() {
        let project = project("uri");
        let item = RawItem {
            name: Some("name".to_owned()),
            ..raw("jdt://contents/rt.jar/java.lang/String.class", 3)
        };
        let shown = lsp::shown(&project.root, None, vec![item]);
        assert_eq!(shown[0].path, "jdt://contents/rt.jar/java.lang/String.class");
        assert_eq!(shown[0].name, None);
    }

    #[test]
    fn an_unloaded_inside_file_gives_its_line() {
        let project = project("line");
        let file = project.root.join("src/a.rs");
        fs::write(&file, b"one\r\n\xfftwo\nthree").unwrap();
        fs::write(project.root.join("src/long.rs"), "x".repeat(400)).unwrap();
        let shown = lsp::shown(
            &project.root,
            None,
            vec![
                raw(&file, 1),
                raw(&file, 2),
                raw(&file, 3),
                raw(&file, 4),
                raw(project.root.join("src/long.rs"), 1),
            ],
        );
        assert_eq!(shown[0].text.as_deref(), Some("one"), "a CR is the line's ending");
        assert_eq!(shown[1].text.as_deref(), Some("\u{fffd}two"), "lossy, not lost");
        assert_eq!(shown[2].text.as_deref(), Some("three"));
        assert_eq!(shown[3].text, None, "past the end");
        assert_eq!(shown[4].text.as_deref().map(|text| text.chars().count()), Some(300));
        assert_eq!(shown[0].path, "src/a.rs");
    }

    #[test]
    fn a_new_file_keeps_its_buffers_line() {
        let project = project("new-file");
        let new = project.root.join("src/new.rs");
        let item = RawItem {
            text: Some("unsaved".to_owned()),
            text_from: Some(new.clone()),
            name: Some("name".to_owned()),
            ..raw(&new, 1)
        };
        let shown = lsp::shown(&project.root, None, vec![item]);
        assert_eq!(shown[0].path, "src/new.rs");
        assert_eq!(shown[0].text.as_deref(), Some("unsaved"));
        assert_eq!(shown[0].name.as_deref(), Some("name"));
    }

    #[test]
    fn a_fifo_named_by_a_server_gives_no_text_and_does_not_block() {
        let project = project("fifo-item");
        let fifo = project.root.join("pipe");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let root = project.root.clone();
        let job = lsp::worker(move || lsp::shown(&root, None, vec![raw(&fifo, 1)]));
        let shown = job.recv_timeout(Duration::from_secs(5)).expect("an open that returns");
        assert_eq!(shown[0].path, "pipe");
        assert_eq!(shown[0].text, None);
    }

    fn raw_part(name: &str, count: usize, total: u32, file: &Path) -> RawPart {
        RawPart {
            name: name.to_owned(),
            items: (0..count).map(|_| raw(file, 1)).collect(),
            total,
        }
    }

    #[test]
    fn the_cap_prefers_definitions_and_counts_the_rest() {
        let project = project("cap");
        let file = project.root.join("src/a.rs");
        let parts = vec![
            raw_part("references", 200, 500, &file),
            raw_part("implementations", 100, 100, &file),
            raw_part("definitions", 150, 150, &file),
        ];
        let result = the_answer(&project.root, None, "foo", parts, &[], &[]);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(answer["definitions"].as_array().unwrap().len(), 150);
        assert_eq!(answer["implementations"].as_array().unwrap().len(), 50);
        assert_eq!(answer["references"].as_array().unwrap().len(), 0);
        assert_eq!(answer["left_out"], 550);
    }

    #[test]
    fn an_answer_of_control_characters_is_cut_to_fit() {
        let project = project("fit");
        let path = format!("/{}", "\u{1}".repeat(4095));
        let parts = vec![RawPart {
            name: "references".to_owned(),
            items: (0..200).map(|_| raw(&path, 1)).collect(),
            total: 200,
        }];
        let result = the_answer(&project.root, None, "foo", parts, &[], &[]);
        assert!(encoded_len(&result.text) <= ANSWER_MAX_ENCODED);
        let answer: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let told = answer["references"].as_array().unwrap().len();
        assert!((1..200).contains(&told), "{told}");
        assert_eq!(answer["left_out"], 200 - told);
    }

    #[test]
    fn phase_b_tools_are_deferred() {
        const { assert!(!ROW.always_load) };
        let set = client_tool_set(&Switches::on_by_default(), Trusted).unwrap();
        let usages = set.tools.iter().find(|tool| tool.name == "usages").expect("declared");
        assert!(!usages.always_load);
    }

    #[test]
    fn the_tool_off_is_not_declared() {
        let names =
            |switches: &Switches, trust| declared(switches, trust).iter().map(|row| row.name).collect::<Vec<_>>();
        assert!(names(&Switches::on_by_default(), Untrusted).contains(&"usages"));
        let off = Switches::parse(None, &[("agent.editor.tool.usages".to_owned(), "off".to_owned())]).unwrap();
        assert!(!names(&off, Trusted).contains(&"usages"));
    }
}
