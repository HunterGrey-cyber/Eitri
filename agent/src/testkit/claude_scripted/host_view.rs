//! What both scripted hosts read from a projection for the driver: the roster rows, the question records, the
//! answers a driver part states in the provider's terms, and the lines a timeout report adds. The layer (a) host
//! folds the events it was handed into a projection of its own and the layer (b) host reads the tab's, so the
//! two answer the same waits the same way.

use super::driver::{DeclineKind, DriverError, PlanDecision, QuestionAnswer, RosterEntry};
use crate::{
    AgentSessionProjection, InteractionResolution, PlanExecutionMode, PlanPhase, PlanRecord, PlanState, QuestionRecord,
    QuestionState, TaskKind, TaskStatus,
};
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

/// Whether a refusal says the agent did not make the move out of plan mode an approval started
/// (`planning::refusal_undoes_move`).
pub fn refusal_undoes_move(error: &crate::ProviderError) -> bool {
    crate::projection::planning::refusal_undoes_move(error)
}

/// The plan a decision goes to: the oldest one still waiting for the person, or, when none waits, the newest, so
/// a late decision reaches a plan that has gone and is refused there.
pub fn plan_to_decide(projection: &AgentSessionProjection) -> Option<&PlanRecord> {
    projection
        .plans
        .iter()
        .find(|p| p.state == PlanState::Waiting)
        .or_else(|| projection.plans.last())
}

/// What the provider is asked for a driver's decision on `plan`: an approval names the digest of the plan
/// the person read, which here is the one in the record.
pub fn plan_resolution(decision: &PlanDecision, plan: &PlanRecord) -> InteractionResolution {
    let approve = |mode| InteractionResolution::PlanApprove {
        mode,
        plan_sha256: plan.plan_sha256.clone(),
    };
    match decision {
        PlanDecision::ApproveManual => approve(PlanExecutionMode::Manual),
        PlanDecision::ApproveAuto => approve(PlanExecutionMode::Auto),
        PlanDecision::ApproveBypass => approve(PlanExecutionMode::Bypass),
        PlanDecision::Keep { feedback } => InteractionResolution::PlanKeep {
            feedback: feedback.clone(),
        },
    }
}

/// Whether the plan records hold what `want` describes: `{plan: "waiting"|"settled"}` for the newest plan,
/// `{planning: "executing"|"planning"}` for the session's phase as the CLI's reports left it (`planning` once the
/// CLI reported plan mode, `executing` once it is out of it), or `{plan_outcome, mode_after?, feedback?, count?}`
/// for the plans settled with that outcome (at least `count` of them, default 1), `mode_after` and `feedback`
/// being the newest such plan's.
pub fn plan_holds(projection: &AgentSessionProjection, want: &Value) -> Result<bool, DriverError> {
    let Some(object) = want.as_object() else {
        return Err(DriverError::Failed("a plan wait is an object".into()));
    };
    if let Some(state) = object.get("plan") {
        let newest = projection.plans.last().map(|p| &p.state);
        return match state.as_str() {
            Some("waiting") => Ok(matches!(newest, Some(PlanState::Waiting))),
            Some("settled") => Ok(matches!(newest, Some(PlanState::Settled { .. }))),
            other => Err(DriverError::Failed(format!(
                "a plan wait is `waiting` or `settled`, not {other:?}"
            ))),
        };
    }
    if let Some(phase) = object.get("planning") {
        let now = &projection.planning.phase;
        return match phase.as_str() {
            Some("planning") => Ok(*now == PlanPhase::Planning),
            Some("executing") => Ok(*now == PlanPhase::Executing),
            other => Err(DriverError::Failed(format!(
                "a planning wait is `executing` or `planning`, not {other:?}"
            ))),
        };
    }
    if let Some(key) = object
        .keys()
        .find(|k| !["plan_outcome", "mode_after", "feedback", "count"].contains(&k.as_str()))
    {
        return Err(DriverError::Failed(format!("a plan wait has no `{key}`")));
    }
    let outcome = object.get("plan_outcome").and_then(Value::as_str);
    let count = object.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
    let matching: Vec<(&Option<PlanExecutionMode>, &Option<String>)> = projection
        .plans
        .iter()
        .filter_map(|p| match &p.state {
            PlanState::Settled {
                outcome: got,
                mode_after,
                feedback,
                ..
            } if outcome.is_none_or(|want| want == word(got)) => Some((mode_after, feedback)),
            _ => None,
        })
        .collect();
    if matching.len() < count {
        return Ok(false);
    }
    let Some((mode_after, feedback)) = matching.last() else {
        return Ok(false);
    };
    if let Some(want) = object.get("mode_after") {
        let got = mode_after.as_ref().map(word);
        if want.as_str().map(str::to_owned) != got {
            return Ok(false);
        }
    }
    if let Some(want) = object.get("feedback") {
        if want.as_str() != feedback.as_deref() {
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
    let plans: Vec<Value> = projection
        .plans
        .iter()
        .map(|p| {
            let state = match &p.state {
                PlanState::Waiting => "waiting".to_owned(),
                PlanState::Answering => "answering".to_owned(),
                PlanState::Settled { outcome, .. } => format!("settled:{}", word(outcome)),
            };
            json!({"interaction": p.interaction_id, "state": state})
        })
        .collect();
    format!(
        "turn starts {turns:?}; roster {roster:?}; questions {questions:?}; plans {plans:?}; plan phase {}",
        crate::projection::planning::phase_word(&projection.planning.phase)
    )
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
    fn plan(id: &str, state: PlanState) -> PlanRecord {
        PlanRecord {
            seq: 1,
            interaction_id: id.into(),
            tool_use_id: Some("toolu_exit".into()),
            turn_id: None,
            plan: "# Plan".into(),
            plan_sha256: "ab".repeat(32),
            allowed_prompts: Vec::new(),
            state,
            unsupported_reason: None,
        }
    }

    fn settled_plan(
        outcome: crate::PlanOutcome,
        mode_after: Option<PlanExecutionMode>,
        feedback: Option<&str>,
    ) -> PlanState {
        PlanState::Settled {
            outcome,
            mode_after,
            feedback: feedback.map(str::to_owned),
            reason: None,
        }
    }

    #[test]
    fn a_plan_wait_reads_the_newest_plan_and_the_phase() {
        let mut projection = AgentSessionProjection::default();
        let holds = |p: &AgentSessionProjection, want: Value| plan_holds(p, &want).unwrap();
        assert!(!holds(&projection, json!({"plan": "waiting"})), "no plan yet");
        assert!(holds(&projection, json!({"planning": "executing"})));
        projection.plans.push(plan("p1", PlanState::Waiting));
        assert!(holds(&projection, json!({"plan": "waiting"})));
        assert!(!holds(&projection, json!({"plan": "settled"})));
        projection.plans[0].state = PlanState::Answering;
        assert!(
            !holds(&projection, json!({"plan": "waiting"})),
            "an answer on its way is not waiting"
        );
        projection.plans[0].state = settled_plan(crate::PlanOutcome::KeptPlanning, None, Some("more"));
        assert!(holds(&projection, json!({"plan": "settled"})));
        projection.planning.phase = PlanPhase::Planning;
        assert!(holds(&projection, json!({"planning": "planning"})));
        assert!(!holds(&projection, json!({"planning": "executing"})));
        projection.planning.phase = PlanPhase::Leaving {
            cause: crate::PlanCause::Host,
            since_ms: 0,
            saw_default: false,
        };
        assert!(
            !holds(&projection, json!({"planning": "planning"}))
                && !holds(&projection, json!({"planning": "executing"})),
            "a move under way is neither"
        );
        assert!(plan_holds(&projection, &json!({"plan": "answering"})).is_err());
        assert!(plan_holds(&projection, &json!({"planning": "entering"})).is_err());
    }

    #[test]
    fn a_plan_outcome_wait_compares_outcome_mode_feedback_and_count() {
        let mut projection = AgentSessionProjection::default();
        projection.plans.push(plan(
            "p1",
            settled_plan(crate::PlanOutcome::KeptPlanning, None, Some("also the README")),
        ));
        projection.plans.push(plan(
            "p2",
            settled_plan(crate::PlanOutcome::Approved, Some(PlanExecutionMode::Auto), None),
        ));
        let holds = |want: Value| plan_holds(&projection, &want).unwrap();
        assert!(holds(json!({"plan_outcome": "approved"})));
        assert!(holds(json!({"plan_outcome": "approved", "mode_after": "auto"})));
        assert!(!holds(json!({"plan_outcome": "approved", "mode_after": "manual"})));
        assert!(holds(
            json!({"plan_outcome": "kept_planning", "feedback": "also the README"})
        ));
        assert!(!holds(json!({"plan_outcome": "kept_planning", "feedback": "other"})));
        assert!(!holds(json!({"plan_outcome": "approved", "count": 2})));
        assert!(holds(json!({"plan_outcome": "kept_planning", "mode_after": null})));
        assert!(plan_holds(&projection, &json!({"plan_outcome": "approved", "colour": 1})).is_err());
    }

    #[test]
    fn a_decision_names_the_digest_of_the_plan_it_goes_to_and_a_late_one_the_newest() {
        let mut projection = AgentSessionProjection::default();
        assert!(plan_to_decide(&projection).is_none());
        projection
            .plans
            .push(plan("old", settled_plan(crate::PlanOutcome::KeptPlanning, None, None)));
        assert_eq!(plan_to_decide(&projection).unwrap().interaction_id, "old");
        projection.plans.push(plan("live", PlanState::Waiting));
        projection
            .plans
            .push(plan("newer", settled_plan(crate::PlanOutcome::Interrupted, None, None)));
        let record = plan_to_decide(&projection).unwrap();
        assert_eq!(record.interaction_id, "live", "the oldest plan still waiting");
        assert_eq!(
            plan_resolution(&PlanDecision::ApproveAuto, record),
            InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Auto,
                plan_sha256: "ab".repeat(32)
            }
        );
        assert_eq!(
            plan_resolution(&PlanDecision::Keep { feedback: "x".into() }, record),
            InteractionResolution::PlanKeep { feedback: "x".into() }
        );
    }
}
