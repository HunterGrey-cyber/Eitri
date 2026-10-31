//! Runs a tool's run with no broker and no editor, for a tool's own tests. Nothing is stamped and nothing
//! reaches an nvim: the answers are the test's. A `Step::Call` on a spent budget sends nothing here too, so a
//! run hears what it would hear from the broker.

use std::cell::RefCell;
use std::collections::VecDeque;

use rmpv::Value;

use super::cards::{CardAnswer, CardDecision, CardOrigin, CardRequest};
use super::stamp::STAMP_MARGIN;
use super::step::{EditorFailure, Step, StepEvent, ToolCx, ToolRun};
use super::way::EditorCall;
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Answer, Pending};

/// An editor a test answers by hand, for a test of the path around a tool: a chunk waits here until the test
/// answers it, as one out at a busy nvim does.
#[derive(Default)]
pub struct FakeEditor {
    waiting: RefCell<VecDeque<Answer>>,
    asked: RefCell<Vec<&'static str>>,
}

impl FakeEditor {
    /// Every chunk it was sent, oldest first, answered or not.
    pub fn asked(&self) -> Vec<&'static str> {
        self.asked.borrow().clone()
    }

    /// Answers the oldest chunk still waiting.
    pub fn answer_oldest(&self, reply: &serde_json::Value) {
        let answer = self.waiting.borrow_mut().pop_front().expect("a chunk is waiting");
        answer.send(Ok(value_of(reply)));
    }
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, _args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.asked.borrow_mut().push(code);
        self.waiting.borrow_mut().push_back(answer);
        pending
    }

    fn target(&self) -> Option<u64> {
        Some(1)
    }
}

/// What nvim would send for `reply`.
fn value_of(reply: &serde_json::Value) -> Value {
    match reply {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(value) => Value::from(*value),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(value) => Value::from(value),
            None => Value::from(number.as_f64().expect("a number")),
        },
        serde_json::Value::String(text) => Value::from(text.as_str()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(value_of).collect()),
        serde_json::Value::Object(fields) => Value::Map(
            fields
                .iter()
                .map(|(name, value)| (Value::from(name.as_str()), value_of(value)))
                .collect(),
        ),
    }
}

/// [`drive_with_cards`] for a run that raises no card.
pub fn drive(
    run: Box<dyn ToolRun>,
    cx: &ToolCx<'_>,
    answers: impl FnMut(&EditorCall) -> Result<Value, EditorFailure>,
) -> Vec<Step> {
    drive_with_cards(run, cx, answers, |request| {
        panic!("a card with nothing to answer it: {}", request.title)
    })
}

/// Every step the run took, its last the `Step::Done`.
pub fn drive_with_cards(
    mut run: Box<dyn ToolRun>,
    cx: &ToolCx<'_>,
    mut answers: impl FnMut(&EditorCall) -> Result<Value, EditorFailure>,
    mut cards: impl FnMut(&CardRequest) -> (CardDecision, CardOrigin),
) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut event = StepEvent::Start;
    loop {
        let step = run.step(event, cx);
        event = match &step {
            Step::Done(_) => {
                steps.push(step);
                return steps;
            }
            Step::Call { budget, .. } if *budget <= STAMP_MARGIN => StepEvent::Answer(Err(EditorFailure::NotAnswered)),
            Step::Call { call, .. } => StepEvent::Answer(answers(call)),
            Step::Ask(request) => {
                let (decision, origin) = cards(request);
                StepEvent::Card(CardAnswer::for_tests(request, decision, origin, cx.call))
            }
            Step::PollAfter(_) => StepEvent::Poll,
        };
        steps.push(step);
    }
}
