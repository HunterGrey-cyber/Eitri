//! What both scripted hosts read from a projection for the driver: the roster rows, the question records, the
//! answers a driver part states in the provider's terms, and the lines a timeout report adds. The layer (a) host
//! folds the events it was handed into a projection of its own and the layer (b) host reads the tab's, so the
//! two answer the same waits the same way.

use super::driver::{DeclineKind, DriverError, QuestionAnswer, RosterEntry};
use crate::{AgentSessionProjection, QuestionRecord, QuestionState, TaskKind, TaskStatus};
use serde_json::{json, Value};

/// A value in the words the projection serializes it with (`running`, `local_agent`, `chat_requested`).
fn word<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(word)) => word,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// The roster's rows, live and finished, oldest first.
pub fn roster_of(projection: &AgentSessionProjection) -> Vec<RosterEntry> {
    projection
        .background_tasks
        .rows
        .iter()
        .map(|row| RosterEntry {
            task: row.task.task_id.clone(),
            kind: word::<TaskKind>(&row.task.kind),
            status: word::<TaskStatus>(&row.status),
            ambient: row.task.ambient,
        })
        .collect()
}

/// A provider error as a part's refusal text names it: a typed refusal as the wire's code in capitals and the
/// provider's own words (`INTERACTION_NOT_FOUND: ...`), anything else as it prints.
pub fn refusal_text(error: &crate::ProviderError) -> String {
    match error {
        crate::ProviderError::Provider { code, message } => {
            let name = format!("{code:?}");
            let mut shouted = String::new();
            for (i, c) in name.chars().enumerate() {
                if c.is_uppercase() && i > 0 {
                    shouted.push('_');
                }
                shouted.push(c.to_ascii_uppercase());
            }
            format!("{shouted}: {message}")
        }
        other => other.to_string(),
    }
}

/// The answers of a driver part as the provider takes them.
pub fn provider_answers(answers: &[QuestionAnswer]) -> Vec<crate::QuestionAnswer> {
    answers
        .iter()
        .map(|a| crate::QuestionAnswer {
            question_index: a.question as u32,
            option_indexes: a.options.iter().map(|&i| i as u32).collect(),
            other_text: a.other.clone(),
            notes: a.notes.clone(),
        })
        .collect()
}

/// How the provider names a decline.
pub fn decline_kind(kind: DeclineKind) -> crate::QuestionDeclineKind {
    match kind {
        DeclineKind::Cancel => crate::QuestionDeclineKind::Cancel,
        DeclineKind::Chat => crate::QuestionDeclineKind::Chat,
    }
}

/// The question set an answer goes to: the oldest one still waiting for the person, or, when none waits, the
/// newest, so a late answer reaches a question that has gone and is refused there.
pub fn question_to_answer(projection: &AgentSessionProjection) -> Option<&QuestionRecord> {
    projection
        .questions
        .iter()
        .find(|q| q.state == QuestionState::Waiting)
        .or_else(|| projection.questions.last())
}

/// Whether the question records hold what `want` describes: `{question: "waiting"|"settled"}` for the newest
/// record, or `{outcome, answers?, notes?, count?}` for the records settled with that outcome (at least `count`
/// of them, default 1), `answers` and `notes` being those of the newest such record: each answered question's
/// text, and the notes of the answers that carry any.
pub fn interaction_holds(projection: &AgentSessionProjection, want: &Value) -> Result<bool, DriverError> {
    let Some(object) = want.as_object() else {
        return Err(DriverError::Failed("an interaction wait is an object".into()));
    };
    if let Some(state) = object.get("question") {
        let newest = projection.questions.last().map(|q| &q.state);
        return match state.as_str() {
            Some("waiting") => Ok(matches!(newest, Some(QuestionState::Waiting))),
            Some("settled") => Ok(matches!(newest, Some(QuestionState::Settled { .. }))),
            other => Err(DriverError::Failed(format!(
                "a question wait is `waiting` or `settled`, not {other:?}"
            ))),
        };
    }
    if let Some(key) = object
        .keys()
        .find(|k| !["outcome", "answers", "notes", "count"].contains(&k.as_str()))
    {
        return Err(DriverError::Failed(format!("an interaction wait has no `{key}`")));
    }
    let outcome = object.get("outcome").and_then(Value::as_str);
    let count = object.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
    let matching: Vec<(&Vec<crate::ShownAnswer>, &crate::QuestionOutcome)> = projection
        .questions
        .iter()
        .filter_map(|q| match &q.state {
            QuestionState::Settled {
                outcome: got, answers, ..
            } if outcome.is_none_or(|want| want == word(got)) => Some((answers, got)),
            _ => None,
        })
        .collect();
    if matching.len() < count {
        return Ok(false);
    }
    let Some((answers, _)) = matching.last() else {
        return Ok(false);
    };
    if let Some(want) = object.get("answers") {
        let got: Vec<&str> = answers.iter().map(|a| a.answer.as_str()).collect();
        let want: Vec<&str> = want
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if got != want {
            return Ok(false);
        }
    }
    if let Some(want) = object.get("notes") {
        let got: Vec<&str> = answers.iter().filter_map(|a| a.notes.as_deref()).collect();
        let want: Vec<&str> = want
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if got != want {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The lines of a timeout report that come from the projection: each turn's origin and cause, the roster and the
/// question records.
pub fn report_lines(projection: &AgentSessionProjection) -> String {
    let turns: Vec<Value> = projection
        .turn_starts
        .iter()
        .map(|t| {
            json!({
                "turn": t.turn_id,
                "origin": word(&t.origin),
                "cause": t.cause.as_ref().map(|c| word(&c.kind)),
            })
        })
        .collect();
    let roster: Vec<Value> = roster_of(projection)
        .into_iter()
        .map(|r| json!({"task": r.task, "kind": r.kind, "status": r.status, "ambient": r.ambient}))
        .collect();
    let questions: Vec<Value> = projection
        .questions
        .iter()
        .map(|q| {
            let state = match &q.state {
                QuestionState::Waiting => "waiting".to_owned(),
                QuestionState::Answering => "answering".to_owned(),
                QuestionState::Settled { outcome, .. } => format!("settled:{}", word(outcome)),
            };
            json!({"interaction": q.interaction_id, "state": state})
        })
        .collect();
    format!("turn starts {turns:?}; roster {roster:?}; questions {questions:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QuestionOutcome, ShownAnswer};

    fn record(id: &str, state: QuestionState) -> QuestionRecord {
        QuestionRecord {
            seq: 1,
            interaction_id: id.into(),
            tool_use_id: None,
            turn_id: None,
            questions: Vec::new(),
            state,
            unsupported_reason: None,
        }
    }

    fn settled(outcome: QuestionOutcome, answers: &[(&str, Option<&str>)]) -> QuestionState {
        QuestionState::Settled {
            outcome,
            answers: answers
                .iter()
                .enumerate()
                .map(|(i, (answer, notes))| ShownAnswer {
                    question_index: i as u32,
                    answer: (*answer).into(),
                    notes: notes.map(str::to_owned),
                })
                .collect(),
            reason: None,
        }
    }

    #[test]
    fn a_question_wait_reads_the_newest_record() {
        let mut projection = AgentSessionProjection::default();
        assert_eq!(
            interaction_holds(&projection, &json!({"question": "waiting"})),
            Ok(false),
            "no question yet"
        );
        projection.questions.push(record("q1", QuestionState::Waiting));
        assert_eq!(
            interaction_holds(&projection, &json!({"question": "waiting"})),
            Ok(true)
        );
        assert_eq!(
            interaction_holds(&projection, &json!({"question": "settled"})),
            Ok(false)
        );
        projection.questions[0].state = settled(QuestionOutcome::Declined, &[]);
        assert_eq!(
            interaction_holds(&projection, &json!({"question": "waiting"})),
            Ok(false)
        );
        assert_eq!(
            interaction_holds(&projection, &json!({"question": "settled"})),
            Ok(true)
        );
        assert!(interaction_holds(&projection, &json!({"question": "answering"})).is_err());
    }

    #[test]
    fn an_interaction_wait_compares_outcome_answers_notes_and_count() {
        let mut projection = AgentSessionProjection::default();
        projection.questions.push(record(
            "q1",
            settled(
                QuestionOutcome::Answered,
                &[("Blue", None), ("List", Some("keep it short"))],
            ),
        ));
        let holds = |want: Value| interaction_holds(&projection, &want).unwrap();
        assert!(holds(json!({"outcome": "answered"})));
        assert!(holds(json!({"outcome": "answered", "answers": ["Blue", "List"]})));
        assert!(!holds(json!({"outcome": "answered", "answers": ["Blue"]})));
        assert!(holds(json!({"outcome": "answered", "notes": ["keep it short"]})));
        assert!(!holds(json!({"outcome": "declined"})));
        assert!(!holds(json!({"outcome": "answered", "count": 2})));
        assert!(interaction_holds(&projection, &json!({"outcome": "answered", "colour": 1})).is_err());
    }

    #[test]
    fn a_late_answer_goes_to_the_newest_record_when_none_waits() {
        let mut projection = AgentSessionProjection::default();
        assert!(question_to_answer(&projection).is_none());
        projection
            .questions
            .push(record("old", settled(QuestionOutcome::Answered, &[])));
        projection
            .questions
            .push(record("gone", settled(QuestionOutcome::Interrupted, &[])));
        assert_eq!(question_to_answer(&projection).unwrap().interaction_id, "gone");
        projection.questions.push(record("live", QuestionState::Waiting));
        projection
            .questions
            .push(record("newer", settled(QuestionOutcome::Declined, &[])));
        assert_eq!(
            question_to_answer(&projection).unwrap().interaction_id,
            "live",
            "the oldest record still waiting"
        );
    }

    #[test]
    fn a_typed_refusal_is_named_by_the_wire_code() {
        let error = crate::ProviderError::Provider {
            code: crate::ProviderErrorCode::InteractionNotFound,
            message: "gone".into(),
        };
        assert_eq!(refusal_text(&error), "INTERACTION_NOT_FOUND: gone");
        assert_eq!(
            refusal_text(&crate::ProviderError::Timeout),
            crate::ProviderError::Timeout.to_string()
        );
    }

    #[test]
    fn a_driver_answer_becomes_the_providers() {
        let answers = provider_answers(&[QuestionAnswer {
            question: 1,
            options: vec![0, 2],
            other: Some("text".into()),
            notes: None,
        }]);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].question_index, 1);
        assert_eq!(answers[0].option_indexes, [0, 2]);
        assert_eq!(answers[0].other_text.as_deref(), Some("text"));
        assert_eq!(decline_kind(DeclineKind::Chat), crate::QuestionDeclineKind::Chat);
    }
}
