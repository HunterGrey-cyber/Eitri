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
    /// Each chunk's arguments after its stamp, as JSON.
    args: RefCell<Vec<serde_json::Value>>,
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

    /// Answers the chunk sent last of those still waiting.
    pub fn answer_newest(&self, reply: &serde_json::Value) {
        let answer = self.waiting.borrow_mut().pop_back().expect("a chunk is waiting");
        answer.send(Ok(value_of(reply)));
    }

    /// Fails the chunk sent last of those still waiting, as a connection that ended would.
    pub fn fail_newest(&self) {
        let answer = self.waiting.borrow_mut().pop_back().expect("a chunk is waiting");
        answer.send(Err(crate::nvim_rpc::RpcError::Closed));
    }

    /// How many chunks wait for their answer.
    pub fn unanswered(&self) -> usize {
        self.waiting.borrow().len()
    }

    /// The `index`th chunk's first argument after its stamp (the request's map), as JSON: a string that is not UTF-8
    /// and a binary value read lossily, a map's keys as strings.
    pub fn args(&self, index: usize) -> serde_json::Value {
        self.args.borrow()[index].clone()
    }
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.asked.borrow_mut().push(code);
        self.args
            .borrow_mut()
            .push(args.get(1).map(json_of).unwrap_or(serde_json::Value::Null));
        self.waiting.borrow_mut().push_back(answer);
        pending
    }

    fn target(&self) -> Option<u64> {
        Some(1)
    }
}

/// What a test reads of a value sent to nvim.
fn json_of(value: &Value) -> serde_json::Value {
    match value {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(value) => serde_json::Value::from(*value),
        Value::Integer(number) => number
            .as_u64()
            .map(serde_json::Value::from)
            .or_else(|| number.as_i64().map(serde_json::Value::from))
            .unwrap_or(serde_json::Value::Null),
        Value::F32(number) => serde_json::Value::from(f64::from(*number)),
        Value::F64(number) => serde_json::Value::from(*number),
        Value::String(text) => serde_json::Value::from(String::from_utf8_lossy(text.as_bytes()).into_owned()),
        Value::Binary(bytes) => serde_json::Value::from(String::from_utf8_lossy(bytes).into_owned()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(json_of).collect()),
        Value::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, value)| {
                    let key = match key {
                        Value::String(text) => String::from_utf8_lossy(text.as_bytes()).into_owned(),
                        other => other.to_string(),
                    };
                    (key, json_of(value))
                })
                .collect(),
        ),
        Value::Ext(_, bytes) => serde_json::Value::from(String::from_utf8_lossy(bytes).into_owned()),
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
