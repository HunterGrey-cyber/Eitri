//! What a tab's diagnostics were when its turn began, for `diagnostics {since: "turn"}` to subtract. The baseline
//! is the `diagnostics` chunk asked once for the whole project; this file keeps its entries as a multiset and
//! subtracts it from what the editor says later.

use std::collections::BTreeMap;

use rmpv::Value;

use super::diagnostics::{self, DiagnosticEntry, Severity};
use crate::editor_tools::stamp::EDITOR_BUDGET;
use crate::editor_tools::step::{Step, StepEvent, ToolCx, ToolResult, ToolRun};
use crate::editor_tools::way::EditorCall;

/// The most entries a baseline, and the later read it is subtracted from, hold.
pub const BASELINE_LIMIT: u32 = 2000;

/// What a `since` call says when the turn has no baseline: it was not taken, is not in yet, or the tab moved in
/// mid-turn.
pub const NO_BASELINE: &str = "no baseline for this turn";

/// What makes two entries the same one. The lines are left out: an edit above an entry moves it, and it is the
/// same entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BaselineKey {
    pub path: String,
    pub severity: u8,
    pub source: String,
    pub code: String,
    pub message: String,
}

impl BaselineKey {
    pub fn of(entry: &DiagnosticEntry) -> BaselineKey {
        BaselineKey {
            path: entry.path.to_string_lossy().into_owned(),
            severity: match entry.severity {
                Severity::Error => 1,
                Severity::Warning => 2,
                Severity::Info => 3,
                Severity::Hint => 4,
            },
            source: entry.source.clone().unwrap_or_default(),
            code: entry.code.clone().unwrap_or_default(),
            message: entry.message.clone(),
        }
    }
}

/// Where a tab's baseline for its newest turn stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnBaseline {
    /// Asked of the editor, not answered yet.
    Taking { turn: String },
    /// `cut`: the chunk stopped at [`BASELINE_LIMIT`] or at its size, so some entries are not in `counts`.
    Taken {
        turn: String,
        counts: BTreeMap<BaselineKey, u32>,
        cut: bool,
    },
    /// The editor did not answer, or answered something unreadable.
    Failed { turn: String },
}

impl TurnBaseline {
    pub fn turn(&self) -> &str {
        match self {
            TurnBaseline::Taking { turn } | TurnBaseline::Taken { turn, .. } | TurnBaseline::Failed { turn } => turn,
        }
    }
}

/// The entries that appeared: each key's first `counts[key]` occurrences are taken out, and the rest are returned
/// in their order.
pub fn appeared(entries: Vec<DiagnosticEntry>, counts: &BTreeMap<BaselineKey, u32>) -> Vec<DiagnosticEntry> {
    let mut left = counts.clone();
    entries
        .into_iter()
        .filter(|entry| match left.get_mut(&BaselineKey::of(entry)) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// The multiset of an answer's entries.
pub fn counted(entries: &[DiagnosticEntry]) -> BTreeMap<BaselineKey, u32> {
    let mut counts = BTreeMap::new();
    for entry in entries {
        *counts.entry(BaselineKey::of(entry)).or_insert(0) += 1;
    }
    counts
}

/// A run the host starts at a turn's beginning. It answers nobody; its outcome is the tab's memory.
pub struct BaselineRun {
    turn: String,
}

impl BaselineRun {
    pub fn new(turn: &str) -> BaselineRun {
        BaselineRun { turn: turn.to_owned() }
    }

    /// Keeps `outcome` if the tab still waits for this turn's baseline: a newer turn, or a tab that went and came
    /// back, has its own.
    fn settle(&self, cx: &ToolCx<'_>, outcome: TurnBaseline) {
        cx.memory.with(cx.tab, |memory| {
            if matches!(&memory.baseline, Some(TurnBaseline::Taking { turn }) if *turn == self.turn) {
                memory.baseline = Some(outcome);
            }
        });
    }

    fn failed(&self) -> TurnBaseline {
        TurnBaseline::Failed {
            turn: self.turn.clone(),
        }
    }

    fn taken(&self, value: &Value) -> TurnBaseline {
        match diagnostics::parse(value) {
            Ok(answer) => TurnBaseline::Taken {
                turn: self.turn.clone(),
                counts: counted(&answer.entries),
                cut: answer.left_out > 0,
            },
            Err(_) => self.failed(),
        }
    }
}

impl ToolRun for BaselineRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        match event {
            StepEvent::Start => Step::Call {
                call: EditorCall::Baseline {
                    root: cx.root.to_path_buf(),
                },
                budget: EDITOR_BUDGET,
            },
            StepEvent::Answer(answer) => {
                let outcome = match answer {
                    Ok(value) => self.taken(&value),
                    Err(_) => self.failed(),
                };
                self.settle(cx, outcome);
                Step::Done(nothing())
            }
            StepEvent::Card(_) | StepEvent::Poll => Step::Done(nothing()),
        }
    }
}

fn nothing() -> ToolResult {
    ToolResult {
        text: String::new(),
        is_error: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::memory::Memory;
    use crate::editor_tools::step::{AgentCall, EditorFailure, EditorReach, EditorSight};
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use std::path::{Path, PathBuf};

    fn entry(path: &str, line: u32, severity: Severity, message: &str) -> DiagnosticEntry {
        DiagnosticEntry {
            path: PathBuf::from(path),
            line,
            column: 1,
            end_line: line,
            end_column: 2,
            severity,
            source: Some("rustc".to_owned()),
            code: None,
            message: message.to_owned(),
            start_character: None,
            end_character: None,
        }
    }

    fn messages(entries: &[DiagnosticEntry]) -> Vec<(&str, u32)> {
        entries
            .iter()
            .map(|entry| (entry.message.as_str(), entry.line))
            .collect()
    }

    #[test]
    fn appeared_is_a_multiset_difference() {
        let before = [
            entry("/p/a.rs", 3, Severity::Error, "twice"),
            entry("/p/a.rs", 9, Severity::Error, "twice"),
            entry("/p/a.rs", 5, Severity::Warning, "gone"),
        ];
        let counts = counted(&before);
        let now = vec![
            entry("/p/a.rs", 4, Severity::Error, "twice"),
            entry("/p/a.rs", 10, Severity::Error, "twice"),
            entry("/p/a.rs", 20, Severity::Error, "twice"),
            entry("/p/b.rs", 1, Severity::Error, "twice"),
            entry("/p/a.rs", 2, Severity::Warning, "new"),
        ];
        let new = appeared(now, &counts);
        assert_eq!(messages(&new), [("twice", 20), ("twice", 1), ("new", 2)]);
        assert_eq!(
            new[1].path,
            PathBuf::from("/p/b.rs"),
            "the same text in another file is new"
        );
        assert!(
            appeared(vec![], &counts).is_empty(),
            "an entry that went is not reported"
        );
    }

    #[test]
    fn the_key_ignores_lines_and_not_severity() {
        let a = entry("/p/a.rs", 3, Severity::Error, "m");
        let moved = entry("/p/a.rs", 40, Severity::Error, "m");
        let milder = entry("/p/a.rs", 3, Severity::Warning, "m");
        assert_eq!(BaselineKey::of(&a), BaselineKey::of(&moved));
        assert_ne!(BaselineKey::of(&a), BaselineKey::of(&milder));
    }

    fn with_cx<R>(memory: &Memory, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: Path::new("/p"),
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 7,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory,
        };
        f(&cx)
    }

    fn reply(entries: Vec<(&str, u32, &str)>, left_out: u32) -> Value {
        let map =
            |fields: Vec<(&str, Value)>| Value::Map(fields.into_iter().map(|(k, v)| (Value::from(k), v)).collect());
        map(vec![
            (
                "entries",
                Value::Array(
                    entries
                        .into_iter()
                        .map(|(path, lnum, message)| {
                            map(vec![
                                ("path", Value::from(path)),
                                ("lnum", Value::from(lnum)),
                                ("col", Value::from(0)),
                                ("end_lnum", Value::from(lnum)),
                                ("end_col", Value::from(1)),
                                ("severity", Value::from("error")),
                                ("message", Value::from(message)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("modified", Value::Array(vec![])),
            ("left_out", Value::from(left_out)),
        ])
    }

    fn baseline_of(memory: &Memory) -> Option<TurnBaseline> {
        memory.with(7, |tab| tab.baseline.clone())
    }

    fn taking(memory: &Memory, turn: &str) {
        memory.with(7, |tab| {
            tab.baseline = Some(TurnBaseline::Taking { turn: turn.to_owned() })
        });
    }

    #[test]
    fn the_run_asks_once_and_keeps_what_it_hears() {
        let memory = Memory::default();
        taking(&memory, "t1");
        let mut run = BaselineRun::new("t1");
        with_cx(&memory, |cx| {
            let Step::Call { call, .. } = run.step(StepEvent::Start, cx) else {
                panic!("the run asks the editor");
            };
            assert!(matches!(call, EditorCall::Baseline { ref root } if root == Path::new("/p")));
            let done = run.step(
                StepEvent::Answer(Ok(reply(vec![("/p/a.rs", 1, "m"), ("/p/a.rs", 5, "m")], 3))),
                cx,
            );
            assert!(matches!(done, Step::Done(ToolResult { is_error: false, .. })));
        });
        let Some(TurnBaseline::Taken { turn, counts, cut }) = baseline_of(&memory) else {
            panic!("a baseline was taken");
        };
        assert_eq!((turn.as_str(), cut), ("t1", true), "a cut side is said");
        assert_eq!(counts.values().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn any_failure_leaves_the_turn_without_a_baseline() {
        for failure in [
            Err(EditorFailure::NotAnswered),
            Err(EditorFailure::Unavailable("down".to_owned())),
            Err(EditorFailure::Refused("no".to_owned())),
            Ok(Value::Nil),
            Ok(Value::Map(vec![(Value::from("expired"), Value::from(true))])),
        ] {
            let memory = Memory::default();
            taking(&memory, "t1");
            let mut run = BaselineRun::new("t1");
            with_cx(&memory, |cx| {
                run.step(StepEvent::Start, cx);
                run.step(StepEvent::Answer(failure), cx);
            });
            assert_eq!(
                baseline_of(&memory),
                Some(TurnBaseline::Failed { turn: "t1".to_owned() })
            );
        }
    }

    #[test]
    fn an_answer_for_an_older_turn_changes_nothing() {
        let memory = Memory::default();
        taking(&memory, "t2");
        let mut run = BaselineRun::new("t1");
        with_cx(&memory, |cx| {
            run.step(StepEvent::Start, cx);
            run.step(StepEvent::Answer(Ok(reply(vec![], 0))), cx);
        });
        assert_eq!(
            baseline_of(&memory),
            Some(TurnBaseline::Taking { turn: "t2".to_owned() })
        );
        // Nor does one for a tab whose memory went.
        let gone = Memory::default();
        let mut run = BaselineRun::new("t1");
        with_cx(&gone, |cx| {
            run.step(StepEvent::Start, cx);
            run.step(StepEvent::Answer(Ok(reply(vec![], 0))), cx);
        });
        assert_eq!(baseline_of(&gone), None);
    }
}
