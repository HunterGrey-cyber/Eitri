use super::{AgentDomainEvent, AgentSessionProjection, QuestionOutcome, QuestionRecord, QuestionSpec, QuestionState};
use crate::provider::{InteractionResolution, QuestionAnswer, QuestionDeclineKind};
use serde_json::{json, Value};

/// The longest one question's answer may be as it goes to Claude, in UTF-16 code units (the CLI's
/// count). The sidecar holds every answer to the same limits; they are checked here first so an
/// answer that would be refused is never sent.
const ANSWER_MAX_UNITS: usize = 32768;
/// The longest every answer of one set may be together, as they go to Claude.
const ANSWERS_MAX_UNITS: usize = 32768;
/// The longest the notes on one question may be.
const NOTES_MAX_UNITS: usize = 8192;

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Whether a question takes notes: a single-select question that offers a preview.
fn takes_notes(question: &QuestionSpec) -> bool {
    !question.multi_select && question.options.iter().any(|option| option.preview.is_some())
}

/// One multi-select item as it goes into the joined answer: quoted as a JSON string when it holds the
/// separator or a double quote, so the CLI's split on `", "` gets the items back whole.
fn join_item(item: &str) -> String {
    if item.contains(", ") || item.contains('"') {
        serde_json::to_string(item).unwrap_or_else(|_| item.to_string())
    } else {
        item.to_string()
    }
}

/// Checks `answers` against the set they answer, rule by rule as the sidecar does, and returns the
/// first rule broken. Nothing is cut or guessed: an answer over a limit is refused whole, and the
/// person still has it to shorten.
fn check_answers(questions: &[QuestionSpec], answers: &[QuestionAnswer]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0usize;
    for answer in answers {
        let index = answer.question_index;
        let question = questions
            .get(index as usize)
            .ok_or_else(|| format!("there is no question {index} in this set of {}", questions.len()))?;
        if !seen.insert(index) {
            return Err(format!("question {index} is answered twice"));
        }
        let mut picked = std::collections::BTreeSet::new();
        for &option in &answer.option_indexes {
            if option as usize >= question.options.len() {
                return Err(format!("question {index} has no option {option}"));
            }
            if !picked.insert(option) {
                return Err(format!("question {index} picks option {option} twice"));
            }
        }
        let text = answer.other_text.as_deref().unwrap_or("");
        if !question.multi_select && picked.len() + usize::from(!text.is_empty()) > 1 {
            return Err(format!("question {index} takes one choice: an option or typed text"));
        }
        let notes = answer.notes.as_deref().unwrap_or("");
        if !notes.is_empty() && !takes_notes(question) {
            return Err(format!("question {index} takes no notes"));
        }
        if utf16_len(notes) > NOTES_MAX_UNITS {
            return Err(format!(
                "the notes on question {index} are longer than {NOTES_MAX_UNITS} characters"
            ));
        }
        if picked.is_empty() && text.is_empty() {
            continue;
        }
        let labels = picked
            .iter()
            .map(|&option| question.options[option as usize].label.as_str());
        let value = if question.multi_select {
            labels
                .chain((!text.is_empty()).then_some(text))
                .map(join_item)
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            labels
                .chain(std::iter::once(text))
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let units = utf16_len(&value);
        if units > ANSWER_MAX_UNITS {
            return Err(format!(
                "the answer to question {index} is longer than {ANSWER_MAX_UNITS} characters"
            ));
        }
        total += units;
    }
    if total > ANSWERS_MAX_UNITS {
        return Err(format!(
            "the answers together are longer than {ANSWERS_MAX_UNITS} characters"
        ));
    }
    Ok(())
}

impl QuestionRecord {
    /// Whether `resolution` may be sent for this question set now, and if not, why, in words the
    /// person is shown. Only a set still waiting, offered as a form, takes one, and only by the rules
    /// it would be held to after sending.
    pub fn check_resolution(&self, resolution: &InteractionResolution) -> Result<(), String> {
        match self.state {
            QuestionState::Waiting => {}
            QuestionState::Answering => return Err("an answer to this question is already on its way".to_string()),
            QuestionState::Settled { .. } => return Err("this question is no longer waiting for an answer".to_string()),
        }
        if self.unsupported_reason.is_some() || self.questions.is_empty() {
            return Err("this question could not be shown, so it takes no answer".to_string());
        }
        match resolution {
            InteractionResolution::Answers(answers) => check_answers(&self.questions, answers),
            InteractionResolution::Decline {
                kind: QuestionDeclineKind::Cancel,
                so_far,
            } if !so_far.is_empty() => Err("a cancel carries no answers; only a request to chat does".to_string()),
            InteractionResolution::Decline { so_far, .. } => check_answers(&self.questions, so_far),
        }
    }
}

impl AgentSessionProjection {
    /// The question set `interaction_id` names, settled or not.
    pub fn question(&self, interaction_id: &str) -> Option<&QuestionRecord> {
        self.questions
            .iter()
            .find(|record| record.interaction_id == interaction_id)
    }

    /// Live question sets, including a send awaiting acknowledgement, in arrival order.
    pub fn waiting_questions(&self) -> impl Iterator<Item = &QuestionRecord> {
        self.questions
            .iter()
            .filter(|record| !matches!(record.state, QuestionState::Settled { .. }))
    }

    /// Marks a local send without claiming that the provider accepted the answer.
    pub fn mark_question_answering(&mut self, interaction_id: &str) -> bool {
        self.change_question_send_state(interaction_id, QuestionState::Waiting, QuestionState::Answering)
    }

    /// A refused or failed send leaves the question available for another answer.
    pub fn question_send_failed(&mut self, interaction_id: &str) -> bool {
        self.change_question_send_state(interaction_id, QuestionState::Answering, QuestionState::Waiting)
    }

    fn change_question_send_state(&mut self, id: &str, from: QuestionState, to: QuestionState) -> bool {
        let Some(record) = self
            .questions
            .iter_mut()
            .find(|record| record.interaction_id == id && record.state == from)
        else {
            return false;
        };
        record.state = to;
        true
    }
}

pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("questions::fold");
    match event {
        AgentDomainEvent::QuestionAsked {
            interaction_id,
            tool_use_id,
            questions,
            unsupported_reason,
        } => {
            state.assistant_message_open = false;
            state.thinking_open = false;
            // Replayed requests replace their earlier copy and take their new arrival position.
            state
                .questions
                .retain(|record| record.interaction_id != *interaction_id);
            state.questions.push(QuestionRecord {
                seq: state.last_revision,
                interaction_id: interaction_id.clone(),
                tool_use_id: tool_use_id.clone(),
                turn_id: state.active_turn_id.clone(),
                questions: questions.clone(),
                state: QuestionState::Waiting,
                unsupported_reason: unsupported_reason.clone(),
            });
        }
        AgentDomainEvent::QuestionSettled {
            interaction_id,
            outcome,
            answers,
            reason,
        } => {
            if let Some(record) = state.questions.iter_mut().find(|record| {
                record.interaction_id == *interaction_id && !matches!(record.state, QuestionState::Settled { .. })
            }) {
                record.state = QuestionState::Settled {
                    outcome: *outcome,
                    answers: answers.clone(),
                    reason: reason.clone(),
                };
            }
        }
        _ => {}
    }
}

/// A call to `tool_use_id` began in turn `turn_id`. The CLI can ask a question before the frames that
/// announce the turn it woke itself for arrive, so the question was folded under the turn before (or
/// under none); the call names the turn it belongs to. A question asked for that call takes the call's
/// turn and its place in the record, and the list stays in `seq` order.
pub(super) fn on_call_started(state: &mut AgentSessionProjection, tool_use_id: &str, turn_id: &str, seq: u64) {
    if tool_use_id.is_empty() {
        return;
    }
    let mut moved = false;
    for record in &mut state.questions {
        if record.tool_use_id.as_deref() == Some(tool_use_id) && record.turn_id.as_deref() != Some(turn_id) {
            record.turn_id = Some(turn_id.to_string());
            record.seq = seq;
            moved = true;
        }
    }
    if moved {
        state.questions.sort_by_key(|record| record.seq);
    }
}

pub(super) fn snapshot_json(state: &AgentSessionProjection, snapshot: &mut Value) {
    #[cfg(test)]
    super::count_hook("questions::snapshot_json");
    if !state.questions.is_empty() {
        snapshot["questions"] = state.questions.iter().map(record_json).collect();
    }
}

fn record_json(record: &QuestionRecord) -> Value {
    let state = match &record.state {
        QuestionState::Waiting => json!({ "kind": "waiting" }),
        QuestionState::Answering => json!({ "kind": "answering" }),
        QuestionState::Settled {
            outcome,
            answers,
            reason,
        } => json!({
            "kind": "settled", "outcome": outcome,
            "answers": answers.iter().map(|answer| json!({
                "questionIndex": answer.question_index, "answer": answer.answer, "notes": answer.notes,
            })).collect::<Vec<_>>(),
            "reason": reason,
        }),
    };
    json!({
        "seq": record.seq, "interactionId": record.interaction_id, "toolUseId": record.tool_use_id,
        "turnId": record.turn_id,
        "questions": record.questions.iter().map(|question| json!({
            "question": question.question, "header": question.header, "multiSelect": question.multi_select,
            "options": question.options.iter().map(|option| json!({
                "label": option.label, "description": option.description, "preview": option.preview,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "state": state, "unsupportedReason": record.unsupported_reason,
    })
}

pub(super) fn on_session_end(state: &mut AgentSessionProjection, _event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("questions::on_session_end");
    for record in &mut state.questions {
        if !matches!(record.state, QuestionState::Settled { .. }) {
            record.state = QuestionState::Settled {
                outcome: QuestionOutcome::SessionClosed,
                answers: vec![],
                reason: None,
            };
        }
    }
}

#[cfg(test)]
#[path = "../testkit/questions_fixture.rs"]
mod tests;

#[cfg(test)]
mod answer_checks {
    use super::*;
    use crate::projection::QuestionOptionSpec;

    fn option(label: &str, preview: Option<&str>) -> QuestionOptionSpec {
        QuestionOptionSpec {
            label: label.into(),
            description: String::new(),
            preview: preview.map(str::to_string),
        }
    }

    /// A single-select question with previews (it takes notes), then a multi-select one.
    fn record(state: QuestionState) -> QuestionRecord {
        QuestionRecord {
            seq: 1,
            interaction_id: "q1".into(),
            tool_use_id: None,
            turn_id: None,
            questions: vec![
                QuestionSpec {
                    question: "Which layout?".into(),
                    header: "Layout".into(),
                    multi_select: false,
                    options: vec![option("Grid", Some("# grid")), option("List", None)],
                },
                QuestionSpec {
                    question: "Which checks?".into(),
                    header: "Checks".into(),
                    multi_select: true,
                    options: vec![option("fmt", None), option("say \"hi\"", None), option("a, b", None)],
                },
            ],
            state,
            unsupported_reason: None,
        }
    }

    fn answer(question_index: u32, option_indexes: &[u32]) -> QuestionAnswer {
        QuestionAnswer {
            question_index,
            option_indexes: option_indexes.to_vec(),
            ..QuestionAnswer::default()
        }
    }

    fn answers(given: Vec<QuestionAnswer>) -> InteractionResolution {
        InteractionResolution::Answers(given)
    }

    #[test]
    fn an_answer_that_keeps_every_rule_of_its_set_is_taken() {
        let waiting = record(QuestionState::Waiting);
        let mut notes = answer(0, &[0]);
        notes.notes = Some("keep the sidebar".into());
        let mut typed = answer(1, &[0, 2]);
        typed.other_text = Some("clippy".into());
        assert_eq!(waiting.check_resolution(&answers(vec![notes, typed])), Ok(()));
        // Answering some questions only, or none, is allowed, as in the terminal's review step.
        assert_eq!(waiting.check_resolution(&answers(vec![answer(1, &[1])])), Ok(()));
        assert_eq!(waiting.check_resolution(&answers(Vec::new())), Ok(()));
        let mut chat = answer(0, &[]);
        chat.other_text = Some("something else".into());
        assert_eq!(
            waiting.check_resolution(&InteractionResolution::Decline {
                kind: QuestionDeclineKind::Chat,
                so_far: vec![chat],
            }),
            Ok(())
        );
        assert_eq!(
            waiting.check_resolution(&InteractionResolution::Decline {
                kind: QuestionDeclineKind::Cancel,
                so_far: Vec::new(),
            }),
            Ok(())
        );
    }

    #[test]
    fn an_answer_breaking_a_rule_of_its_set_is_refused_with_the_rule() {
        let waiting = record(QuestionState::Waiting);
        let refused = |given: Vec<QuestionAnswer>| waiting.check_resolution(&answers(given)).unwrap_err();
        assert!(refused(vec![answer(2, &[0])]).contains("no question 2"));
        assert!(refused(vec![answer(0, &[0]), answer(0, &[1])]).contains("answered twice"));
        assert!(refused(vec![answer(0, &[2])]).contains("no option 2"));
        assert!(refused(vec![answer(1, &[1, 1])]).contains("option 1 twice"));
        assert!(refused(vec![answer(0, &[0, 1])]).contains("one choice"));
        let mut both = answer(0, &[0]);
        both.other_text = Some("typed".into());
        assert!(refused(vec![both]).contains("one choice"));
        let mut multi_notes = answer(1, &[0]);
        multi_notes.notes = Some("why".into());
        assert!(refused(vec![multi_notes]).contains("takes no notes"));
        let mut long_notes = answer(0, &[0]);
        long_notes.notes = Some("n".repeat(NOTES_MAX_UNITS + 1));
        assert!(refused(vec![long_notes]).contains("notes on question 0"));
        // Counted in UTF-16 units, as the CLI counts: a character outside the BMP is two.
        let mut long_text = answer(0, &[]);
        long_text.other_text = Some("\u{1F600}".repeat(ANSWER_MAX_UNITS / 2 + 1));
        assert!(refused(vec![long_text]).contains("answer to question 0"));
        let mut at_limit = answer(0, &[]);
        at_limit.other_text = Some("\u{1F600}".repeat(ANSWER_MAX_UNITS / 2));
        assert_eq!(waiting.check_resolution(&answers(vec![at_limit.clone()])), Ok(()));
        let mut second = answer(1, &[]);
        second.other_text = Some("x".into());
        assert!(refused(vec![at_limit, second]).contains("answers together"));
        assert!(waiting
            .check_resolution(&InteractionResolution::Decline {
                kind: QuestionDeclineKind::Cancel,
                so_far: vec![answer(0, &[0])],
            })
            .unwrap_err()
            .contains("a cancel carries no answers"));
        assert!(waiting
            .check_resolution(&InteractionResolution::Decline {
                kind: QuestionDeclineKind::Chat,
                so_far: vec![answer(0, &[5])],
            })
            .unwrap_err()
            .contains("no option 5"));
    }

    /// A multi-select answer is measured as it goes to Claude: items holding the separator or a quote
    /// are quoted, which can carry it over the limit.
    #[test]
    fn a_multi_select_answer_is_measured_as_joined_and_quoted() {
        assert_eq!(join_item("a, b"), "\"a, b\"");
        assert_eq!(join_item("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(join_item("fmt"), "fmt");
        let waiting = record(QuestionState::Waiting);
        let mut typed = answer(1, &[]);
        // Quoting adds two units, which takes a text exactly at the limit over it.
        typed.other_text = Some(format!("{}, ", "x".repeat(ANSWER_MAX_UNITS - 2)));
        assert!(waiting
            .check_resolution(&answers(vec![typed]))
            .unwrap_err()
            .contains("answer to question 1"));
    }

    #[test]
    fn only_a_question_waiting_as_a_form_takes_an_answer() {
        let given = answers(vec![answer(0, &[0])]);
        assert!(record(QuestionState::Answering)
            .check_resolution(&given)
            .unwrap_err()
            .contains("already on its way"));
        let settled = record(QuestionState::Settled {
            outcome: QuestionOutcome::Expired,
            answers: Vec::new(),
            reason: None,
        });
        assert!(settled
            .check_resolution(&given)
            .unwrap_err()
            .contains("no longer waiting"));
        let mut refused = record(QuestionState::Waiting);
        refused.unsupported_reason = Some("too long".into());
        refused.questions.clear();
        assert!(refused
            .check_resolution(&given)
            .unwrap_err()
            .contains("could not be shown"));
    }
}
