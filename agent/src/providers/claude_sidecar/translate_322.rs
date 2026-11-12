//! Bounded, provider-neutral translation of CLI turns, background work and interactions.
use crate::projection::{
    AgentDomainEvent as Domain, BackgroundTask, ContentKind, NoticeLevel, QuestionOptionSpec, QuestionOutcome,
    QuestionSpec, SessionNoticeKind, ShownAnswer, TaskEdge, TaskKind, TaskStatus, TaskUsage, TurnCause, TurnCauseKind,
    TurnEndDetail, TurnOrigin, WorkflowAgent, WorkflowPhase, WorkflowProgress,
};
use crate::provider::InteractionState;
use claude_runtime_protocol::v1 as wire;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

pub(super) fn bound(text: &str, max: usize) -> String {
    if text.chars().nth(max).is_none() {
        return text.to_owned();
    }
    let mut result: String = text.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}
fn optional(text: Option<String>, max: usize) -> Option<String> {
    text.map(|s| bound(&s, max))
}
fn absent<T>(name: &str) -> Option<T> {
    eprintln!("agent: ClaudeSidecarProvider: absent or unrecognized {name}");
    None
}

fn task_kind(value: i32) -> Option<TaskKind> {
    Some(match wire::TaskKind::try_from(value).ok()? {
        wire::TaskKind::Shell => TaskKind::Shell,
        wire::TaskKind::Monitor => TaskKind::Monitor,
        wire::TaskKind::LocalAgent => TaskKind::LocalAgent,
        wire::TaskKind::Workflow => TaskKind::Workflow,
        wire::TaskKind::Other => TaskKind::Other,
        wire::TaskKind::Unspecified => return absent("TaskKind"),
    })
}
fn task_status(value: i32) -> Option<TaskStatus> {
    Some(match wire::TaskStatus::try_from(value).ok()? {
        wire::TaskStatus::Pending | wire::TaskStatus::Running => TaskStatus::Running,
        wire::TaskStatus::Paused => TaskStatus::Paused,
        wire::TaskStatus::Completed => TaskStatus::Completed,
        wire::TaskStatus::Failed => TaskStatus::Failed,
        wire::TaskStatus::Stopped | wire::TaskStatus::Killed => TaskStatus::Stopped,
        wire::TaskStatus::Unspecified => return absent("TaskStatus"),
    })
}
fn task_edge(value: i32) -> Option<TaskEdge> {
    Some(match wire::TaskEdge::try_from(value).ok()? {
        wire::TaskEdge::Started => TaskEdge::Started,
        wire::TaskEdge::Progress => TaskEdge::Progress,
        wire::TaskEdge::Updated => TaskEdge::Updated,
        wire::TaskEdge::Notified => TaskEdge::Notified,
        wire::TaskEdge::Unspecified => return absent("TaskEdge"),
    })
}
fn word<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("domain enum serializes")
        .as_str()
        .expect("enum is a string")
        .to_owned()
}
fn cause(value: wire::TurnCause) -> Option<TurnCause> {
    let kind = match wire::TurnCauseKind::try_from(value.kind).ok()? {
        wire::TurnCauseKind::Task => TurnCauseKind::Task,
        wire::TurnCauseKind::Scheduled => TurnCauseKind::Scheduled,
        wire::TurnCauseKind::Peer => TurnCauseKind::Peer,
        wire::TurnCauseKind::Other => TurnCauseKind::Other,
        wire::TurnCauseKind::Unknown => TurnCauseKind::Unknown,
        wire::TurnCauseKind::Unspecified => return absent("TurnCauseKind"),
    };
    Some(TurnCause {
        kind,
        task_id: optional(value.task_id, 128),
        tool_use_id: None,
        task_kind: value.task_kind.and_then(task_kind).map(word),
        task_status: value.task_status.and_then(task_status).map(word),
        description: optional(value.description, 500),
        summary: optional(value.summary, 2000),
        peer_name: optional(value.peer_name, 128),
        text: optional(value.text, 2000),
        wakeup_source: optional(value.wakeup_source, 64),
        scheduled_for_ms: value.scheduled_for_ms,
        schedule_reason: optional(value.schedule_reason, 500),
        cron_id: optional(value.cron_id, 128),
        prompt: optional(value.prompt, 2000),
    })
}

pub(super) fn turn_started(started: wire::TurnStarted) -> Vec<Domain> {
    let turn_id = bound(&started.turn_id, 128);
    let mut events = vec![Domain::TurnStarted {
        turn_id: turn_id.clone(),
    }];
    let origin = match wire::TurnOrigin::try_from(started.origin) {
        Ok(wire::TurnOrigin::Client) => None,
        Ok(wire::TurnOrigin::Unspecified) => absent("TurnOrigin"),
        Ok(wire::TurnOrigin::FollowUp) => Some(TurnOrigin::FollowUp),
        Ok(wire::TurnOrigin::Cli) => Some(TurnOrigin::Cli),
        Err(_) => absent("TurnOrigin"),
    };
    if let Some(origin) = origin {
        events.push(Domain::TurnAttributed {
            turn_id,
            origin,
            cause: started.cause.and_then(cause),
        });
    }
    events
}
pub(super) fn turn_detail(completed: &wire::TurnCompleted, detail: &mut TurnEndDetail) {
    detail.origin_kind = optional(completed.origin_kind.clone(), 64);
    detail.num_turns = completed.num_turns;
    detail.reason = optional(detail.reason.take(), 64);
}
pub(super) fn child(parent: Option<String>, frame: Domain) -> Domain {
    match parent {
        Some(parent_tool_use_id) => Domain::SubagentFrame {
            parent_tool_use_id: bound(&parent_tool_use_id, 128),
            frame: Box::new(frame),
        },
        None => frame,
    }
}
/// The reply and thinking text is passed on whole: the sidecar sends a message that was not streamed
/// (every subagent message, a main message whose stream was not observed) as one delta holding the
/// whole block, so cutting it here would cut a reply.
pub(super) fn delta(turn_id: String, text: String, parent: Option<String>, kind: ContentKind) -> Domain {
    child(
        parent,
        Domain::ContentDelta {
            turn_id: bound(&turn_id, 128),
            kind,
            text,
        },
    )
}
/// A task whose kind is unset or unknown stays in the roster as `Other`: the roster is a whole
/// snapshot, so dropping the record would hide a live task.
fn background_task(task: wire::BackgroundTask) -> BackgroundTask {
    let kind = task_kind(task.kind).unwrap_or_else(|| {
        eprintln!(
            "agent: ClaudeSidecarProvider: background task of unrecognized kind {}, shown as other",
            task.kind
        );
        TaskKind::Other
    });
    BackgroundTask {
        task_id: bound(&task.task_id, 128),
        run_id: optional(task.run_id, 128),
        kind,
        task_type: bound(&task.task_type, 64),
        description: bound(&task.description, 500),
        subagent_type: optional(task.subagent_type, 128),
        parent_task_id: optional(task.parent_task_id, 128),
        tool_use_id: optional(task.tool_use_id, 128),
        ambient: task.ambient,
        workflow_name: optional(task.workflow_name, 128),
    }
}
fn workflow(progress: wire::WorkflowProgress) -> WorkflowProgress {
    WorkflowProgress {
        phases: progress
            .phases
            .into_iter()
            .take(64)
            .map(|p| WorkflowPhase {
                index: p.index,
                title: bound(&p.title, 128),
                kind: bound(&p.kind, 128),
            })
            .collect(),
        agents: progress
            .agents
            .into_iter()
            .take(256)
            .map(|a| WorkflowAgent {
                index: a.index,
                label: bound(&a.label, 128),
                phase_index: a.phase_index,
                state: bound(&a.state, 128),
                model: optional(a.model, 128),
                agent_type: optional(a.agent_type, 128),
                tokens: a.tokens,
                tool_calls: a.tool_calls,
                duration_ms: a.duration_ms,
                error: optional(a.error, 500),
            })
            .collect(),
    }
}
pub(crate) fn roster(value: wire::BackgroundTasksChanged, occurred_at: Option<i64>) -> Domain {
    Domain::BackgroundTasksChanged {
        occurred_at,
        tasks: value.tasks.into_iter().take(256).map(background_task).collect(),
    }
}
pub(crate) fn task(value: wire::TaskEvent, occurred_at: Option<i64>) -> Option<Domain> {
    Some(Domain::TaskEvent {
        occurred_at,
        edge: task_edge(value.edge)?,
        task_id: bound(&value.task_id, 128),
        run_id: optional(value.run_id, 128),
        task: value.task.map(background_task),
        status: value.status.and_then(task_status),
        summary: optional(value.summary, 2000),
        last_tool_name: optional(value.last_tool_name, 64),
        usage: value.usage.map(|u| TaskUsage {
            total_tokens: u.total_tokens,
            tool_uses: u.tool_uses,
            duration_ms: u.duration_ms,
        }),
        end_time_ms: value.end_time_ms,
        error: optional(value.error, 2000),
        is_backgrounded: value.is_backgrounded,
        reason: optional(value.reason, 2000),
        output_file: optional(value.output_file, 1024),
        workflow_progress: value.workflow_progress.map(workflow),
    })
}
pub(super) fn cause_updated(value: wire::TurnCauseUpdated) -> Domain {
    Domain::TurnCauseUpdated {
        turn_id: bound(&value.turn_id, 128),
        cause: value.cause.and_then(cause),
    }
}
pub(super) fn cycle(value: wire::CliCycleStarted) -> Domain {
    Domain::CliCycleStarted {
        turn_id: bound(&value.turn_id, 128),
        cause: value.cause.and_then(cause),
    }
}
pub(super) fn reset(value: wire::ConversationReset) -> Domain {
    Domain::ConversationReset {
        new_conversation_id: bound(&value.new_conversation_id, 128),
        trigger: optional(value.trigger, 64),
    }
}
fn notice_level(value: i32) -> NoticeLevel {
    match wire::NoticeLevel::try_from(value) {
        Ok(wire::NoticeLevel::Info) => NoticeLevel::Info,
        Ok(wire::NoticeLevel::Notice) => NoticeLevel::Notice,
        Ok(wire::NoticeLevel::Suggestion) => NoticeLevel::Suggestion,
        Ok(wire::NoticeLevel::Warning) => NoticeLevel::Warning,
        Ok(wire::NoticeLevel::Unspecified) | Err(_) => NoticeLevel::Notice,
    }
}
pub(super) fn notice(value: wire::SessionNotice) -> Option<Domain> {
    let kind = match wire::SessionNoticeKind::try_from(value.kind).ok()? {
        wire::SessionNoticeKind::ApiRetry => SessionNoticeKind::ApiRetry,
        wire::SessionNoticeKind::RateLimit => SessionNoticeKind::RateLimit,
        wire::SessionNoticeKind::ModelFallback => SessionNoticeKind::ModelFallback,
        wire::SessionNoticeKind::ModelRefusal => SessionNoticeKind::ModelRefusal,
        wire::SessionNoticeKind::Compacting => SessionNoticeKind::Compacting,
        wire::SessionNoticeKind::CompactBoundary => SessionNoticeKind::Compaction,
        wire::SessionNoticeKind::Informational => SessionNoticeKind::Informational,
        wire::SessionNoticeKind::Notification => SessionNoticeKind::Notification,
        wire::SessionNoticeKind::PeerHold => SessionNoticeKind::PeerHold,
        wire::SessionNoticeKind::Unspecified => return absent("SessionNoticeKind"),
    };
    if kind == SessionNoticeKind::RateLimit && value.rate_status == Some(wire::RateLimitStatus::Rejected as i32) {
        return Some(Domain::AccountLimits {
            used_percent: value.utilization.filter(|v| v.is_finite()).unwrap_or(1.0) * 100.0,
            window_minutes: None,
            resets_at: value.resets_at_ms.map(|ms| ms.div_euclid(1000)),
            plan: None,
        });
    }
    let mut scalars = BTreeMap::<String, Value>::new();
    macro_rules! scalar { ($($field:ident),* $(,)?) => { $(if let Some(v) = value.$field { scalars.insert(stringify!($field).into(), json!(v)); })* }; }
    macro_rules! text { ($($field:ident : $max:literal),* $(,)?) => { $(if let Some(v) = value.$field { scalars.insert(stringify!($field).into(), json!(bound(&v, $max))); })* }; }
    scalar!(
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        pre_tokens,
        post_tokens,
        resets_at_ms,
        utilization
    );
    text!(from_model:128, to_model:128, trigger:64, peer_name:128, error_kind:64, fallback_scope:64);
    if let Some(status) = value.rate_status.and_then(|s| wire::RateLimitStatus::try_from(s).ok()) {
        let status = match status {
            wire::RateLimitStatus::Allowed => Some("allowed"),
            wire::RateLimitStatus::AllowedWarning => Some("allowed_warning"),
            wire::RateLimitStatus::Rejected => Some("rejected"),
            wire::RateLimitStatus::Unspecified => absent("RateLimitStatus"),
        };
        if let Some(status) = status {
            scalars.insert("rate_status".into(), json!(status));
        }
    }
    if let Some(hold) = value.hold_state.and_then(|s| wire::PeerHoldState::try_from(s).ok()) {
        let hold = match hold {
            wire::PeerHoldState::Held => Some("held"),
            wire::PeerHoldState::Released => Some("released"),
            wire::PeerHoldState::Dropped => Some("dropped"),
            wire::PeerHoldState::Unspecified => absent("PeerHoldState"),
        };
        if let Some(hold) = hold {
            scalars.insert("hold_state".into(), json!(hold));
        }
    }
    Some(Domain::SessionNotice {
        kind,
        level: notice_level(value.level),
        content: optional(value.content, 2000),
        scalars,
        task_id: None,
        tool_use_id: None,
        task_status: None,
    })
}

fn fits(text: &str, max: usize) -> bool {
    text.encode_utf16().take(max + 1).count() <= max
}
fn valid_questions(questions: &[wire::Question]) -> bool {
    let mut texts = HashSet::new();
    if !(1..=4).contains(&questions.len()) {
        return false;
    }
    let mut bytes = 0;
    for question in questions {
        bytes += question.question.len() + question.header.len();
        if !fits(&question.question, 4000)
            || !fits(&question.header, 256)
            || !texts.insert(&question.question)
            || !(2..=4).contains(&question.options.len())
        {
            return false;
        }
        let mut labels = HashSet::new();
        for option in &question.options {
            bytes += option.label.len() + option.description.len() + option.preview.as_ref().map_or(0, String::len);
            if !fits(&option.label, 1000)
                || !fits(&option.description, 4000)
                || option.preview.as_ref().is_some_and(|s| !fits(s, 16384))
                || !labels.insert(&option.label)
            {
                return false;
            }
        }
    }
    // Include JSON syntax and escaping in the aggregate byte budget, not only the visible words.
    let payload: Vec<Value> = questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| {
                    let mut option = json!({"label": o.label, "description": o.description});
                    if let Some(preview) = &o.preview {
                        option["preview"] = json!(preview);
                    }
                    option
                })
                .collect();
            json!({"question": q.question, "header": q.header, "multiSelect": q.multi_select, "options": options})
        })
        .collect();
    bytes <= 256 * 1024 && serde_json::to_vec(&payload).is_ok_and(|v| v.len() <= 256 * 1024)
}
pub(super) fn requested(value: wire::InteractionRequested) -> Domain {
    let mut unsupported = value.unsupported_reason;
    let questions = match (wire::InteractionKind::try_from(value.kind), value.payload) {
        (Ok(wire::InteractionKind::Question), Some(wire::interaction_requested::Payload::Questions(set)))
            if valid_questions(&set.questions) =>
        {
            set.questions
        }
        (Ok(wire::InteractionKind::Question), Some(wire::interaction_requested::Payload::Questions(_))) => {
            unsupported = Some("Question payload exceeds supported bounds or is malformed".into());
            Vec::new()
        }
        // A plan payload on a question is no question payload at all.
        (Ok(wire::InteractionKind::Question), None | Some(wire::interaction_requested::Payload::Plan(_))) => {
            unsupported.get_or_insert_with(|| "Question payload is missing".into());
            Vec::new()
        }
        // A plan approval is never offered by this tree (no `plan_mode` opt-in), so one is refused like any kind
        // it does not know.
        (Ok(wire::InteractionKind::Unspecified | wire::InteractionKind::PlanApproval), _) | (Err(_), _) => {
            unsupported = Some("Unsupported interaction kind".into());
            Vec::new()
        }
    };
    // Identifiers on questions are refused too: cutting one would change what an answer names.
    let ids_fit = fits(&value.interaction_id, 128) && fits(&value.tool_use_id, 128);
    if !ids_fit {
        unsupported = Some("Question identifier exceeds supported bounds".into());
    }
    Domain::QuestionAsked {
        interaction_id: if ids_fit { value.interaction_id } else { String::new() },
        tool_use_id: if ids_fit {
            crate::projection::tool_use_link(value.tool_use_id)
        } else {
            None
        },
        questions: if unsupported.is_some() {
            Vec::new()
        } else {
            questions
                .into_iter()
                .map(|q| QuestionSpec {
                    question: q.question,
                    header: q.header,
                    multi_select: q.multi_select,
                    options: q
                        .options
                        .into_iter()
                        .map(|o| QuestionOptionSpec {
                            label: o.label,
                            description: o.description,
                            preview: o.preview,
                        })
                        .collect(),
                })
                .collect()
        },
        unsupported_reason: optional(unsupported, 2000),
    }
}
fn question_outcome(value: i32) -> Option<QuestionOutcome> {
    Some(match wire::InteractionOutcome::try_from(value).ok()? {
        wire::InteractionOutcome::Answered => QuestionOutcome::Answered,
        wire::InteractionOutcome::Declined => QuestionOutcome::Declined,
        wire::InteractionOutcome::ChatRequested => QuestionOutcome::ChatRequested,
        wire::InteractionOutcome::Interrupted => QuestionOutcome::Interrupted,
        wire::InteractionOutcome::Expired => QuestionOutcome::Expired,
        wire::InteractionOutcome::SessionClosed => QuestionOutcome::SessionClosed,
        wire::InteractionOutcome::ProviderFailed => QuestionOutcome::ProviderFailed,
        wire::InteractionOutcome::HostRefused => QuestionOutcome::HostRefused,
        // Plan outcomes belong to plan approvals, which this tree never takes; on a question they are unreadable.
        wire::InteractionOutcome::Unspecified
        | wire::InteractionOutcome::Approved
        | wire::InteractionOutcome::KeptPlanning => return absent("InteractionOutcome"),
    })
}
pub(super) fn resolved(value: wire::InteractionResolved) -> Option<Domain> {
    if !fits(&value.interaction_id, 128) {
        return absent("interaction identifier");
    }
    let valid = value.answers.len() <= 4
        && value
            .answers
            .iter()
            .all(|a| fits(&a.answer, 32768) && a.notes.as_ref().is_none_or(|n| fits(n, 8192)))
        && value
            .answers
            .iter()
            .map(|a| a.answer.encode_utf16().count())
            .sum::<usize>()
            <= 32768;
    Some(Domain::QuestionSettled {
        interaction_id: value.interaction_id,
        // A settlement whose outcome cannot be read still ends the question, as a failure, never as
        // an answer: dropping it would leave the question waiting for good.
        outcome: if valid {
            question_outcome(value.outcome).unwrap_or(QuestionOutcome::ProviderFailed)
        } else {
            QuestionOutcome::HostRefused
        },
        answers: if valid {
            value
                .answers
                .into_iter()
                .map(|a| ShownAnswer {
                    question_index: a.question_index,
                    answer: a.answer,
                    notes: a.notes,
                })
                .collect()
        } else {
            Vec::new()
        },
        reason: if valid {
            optional(value.reason, 2000)
        } else {
            Some("Question answers exceed supported bounds".into())
        },
    })
}

/// The most of a task's output the sidecar sends, and the most this side keeps: the last bytes.
const OUTPUT_TAIL_BYTES: usize = 8192;
/// The most one ranged read keeps: what the sidecar sends at most.
const OUTPUT_RANGE_BYTES: usize = 1024 * 1024;
/// A model id, a display name or an effort level: words, on the CLI's say-so.
const SETTINGS_WORD_CHARS: usize = 200;
const MODEL_DESCRIPTION_CHARS: usize = 500;
/// More than any CLI has offered; a list past it is cut rather than drawn.
const MODEL_ROWS: usize = 64;
const EFFORT_ROWS: usize = 16;

/// A background task's output as `GetTaskOutput` answered it.
#[allow(dead_code)] // read by the provider's task-output request, which no caller makes yet
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskOutputTail {
    /// The tail, decoded lossily: it can begin in the middle of an encoded character.
    pub output: String,
    /// The size of the whole output, as the sidecar measured it.
    pub total_bytes: u64,
    /// Whether `output` is only the end of it.
    pub truncated: bool,
    /// Where `output` starts in the whole output.
    pub offset: u64,
}

// Each response is destructured field by field, so a field a later protocol adds fails to compile here
// instead of being dropped unseen.

/// A tail longer than the sidecar's own bound keeps its last bytes, since it is the end that is wanted.
pub(super) fn task_output(value: wire::GetTaskOutputResponse) -> TaskOutputTail {
    let wire::GetTaskOutputResponse {
        output,
        total_bytes,
        truncated,
        offset,
    } = value;
    let over = output.len().saturating_sub(OUTPUT_TAIL_BYTES);
    if over > 0 {
        eprintln!(
            "agent: ClaudeSidecarProvider: task output tail of {} bytes, keeping the last {OUTPUT_TAIL_BYTES}",
            output.len()
        );
    }
    TaskOutputTail {
        output: String::from_utf8_lossy(&output[over..]).into_owned(),
        total_bytes,
        truncated: truncated || over > 0,
        offset: offset + over as u64,
    }
}

/// A range of a task's output, for the panel's whole-output view: bytes past `OUTPUT_RANGE_BYTES` are
/// cut from the end, since the range was asked for from its start.
pub(super) fn task_output_range(value: wire::GetTaskOutputResponse) -> TaskOutputTail {
    let wire::GetTaskOutputResponse {
        output,
        total_bytes,
        truncated,
        offset,
    } = value;
    let kept = output.len().min(OUTPUT_RANGE_BYTES);
    TaskOutputTail {
        output: String::from_utf8_lossy(&output[..kept]).into_owned(),
        total_bytes,
        truncated: truncated || kept < output.len(),
        offset,
    }
}

/// What the CLI says it will use. A response with no such message read nothing.
pub(super) fn applied_settings(value: Option<wire::AppliedSettings>) -> crate::AppliedSettings {
    let wire::AppliedSettings { model, effort } = value.unwrap_or_default();
    crate::AppliedSettings {
        model: bound(&model, SETTINGS_WORD_CHARS),
        effort: bound(&effort, SETTINGS_WORD_CHARS),
    }
}

/// The CLI's list of models, kept to what a picker can show. Every word is the CLI's; none decides
/// anything.
pub(super) fn model_list(value: wire::ListModelsResponse) -> crate::ModelList {
    let wire::ListModelsResponse { models, applied } = value;
    crate::ModelList {
        models: models
            .into_iter()
            .take(MODEL_ROWS)
            .map(|row| {
                let wire::ModelOption {
                    value,
                    resolved_model,
                    display_name,
                    description,
                    supports_effort,
                    supported_effort_levels,
                    supports_auto_mode,
                } = row;
                crate::ModelOption {
                    value: bound(&value, SETTINGS_WORD_CHARS),
                    resolved_model: bound(&resolved_model, SETTINGS_WORD_CHARS),
                    display_name: bound(&display_name, SETTINGS_WORD_CHARS),
                    description: bound(&description, MODEL_DESCRIPTION_CHARS),
                    supports_effort,
                    supported_effort_levels: supported_effort_levels
                        .into_iter()
                        .take(EFFORT_ROWS)
                        .map(|level| bound(&level, SETTINGS_WORD_CHARS))
                        .collect(),
                    supports_auto_mode,
                }
            })
            .collect(),
        applied: applied_settings(applied),
        efforts: Vec::new(),
    }
}
/// `StopTask` answers with nothing: a stop the sidecar accepted is the call's success.
pub(super) fn task_stopped(value: wire::StopTaskResponse) {
    let wire::StopTaskResponse {} = value;
}
/// The outcome `ResolveInteraction` reports for the answer it took; unset or unknown is none, and the
/// question's own `InteractionResolved` still settles it.
pub(super) fn resolve_outcome(value: wire::ResolveInteractionResponse) -> Option<QuestionOutcome> {
    let wire::ResolveInteractionResponse { outcome } = value;
    question_outcome(outcome)
}
/// The questions still waiting, as `GetInteractionState` answered after the stream was opened again:
/// each pending request is checked like a live `InteractionRequested`.
pub(super) fn interaction_state(value: wire::GetInteractionStateResponse) -> InteractionState {
    let wire::GetInteractionStateResponse {
        through_sequence,
        pending,
    } = value;
    InteractionState {
        through_sequence,
        pending: pending.into_iter().map(requested).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::session_event::Event;

    fn translate(event: Event) -> Vec<Domain> {
        super::super::translate::translate(
            wire::SessionEvent {
                event: Some(event),
                ..Default::default()
            },
            crate::RequestedCliMode::Default,
        )
    }
    fn question() -> wire::InteractionRequested {
        wire::InteractionRequested {
            interaction_id: "i1".into(),
            tool_use_id: "tool1".into(),
            kind: wire::InteractionKind::Question as i32,
            payload: Some(wire::interaction_requested::Payload::Questions(wire::QuestionSet {
                questions: vec![wire::Question {
                    question: "Which?".into(),
                    header: "Pick".into(),
                    multi_select: false,
                    options: vec![
                        wire::QuestionOption {
                            label: "A".into(),
                            description: "First".into(),
                            preview: Some("**preview**".into()),
                        },
                        wire::QuestionOption {
                            label: "B".into(),
                            ..Default::default()
                        },
                    ],
                }],
            })),
            unsupported_reason: None,
        }
    }
    fn edit_question(value: &mut wire::InteractionRequested) -> &mut wire::Question {
        let Some(wire::interaction_requested::Payload::Questions(set)) = value.payload.as_mut() else {
            panic!("question set")
        };
        &mut set.questions[0]
    }
    fn one(event: Event) -> Domain {
        let mut events = translate(event);
        assert_eq!(events.len(), 1);
        events.remove(0)
    }

    #[test]
    fn rejected_rate_limit_preserves_usage_and_reset_seconds() {
        assert_eq!(
            notice(wire::SessionNotice {
                kind: wire::SessionNoticeKind::RateLimit as i32,
                rate_status: Some(wire::RateLimitStatus::Rejected as i32),
                utilization: Some(0.95),
                resets_at_ms: Some(1_800_000_123_456),
                ..Default::default()
            }),
            Some(Domain::AccountLimits {
                used_percent: 95.0,
                window_minutes: None,
                resets_at: Some(1_800_000_123),
                plan: None,
            })
        );
    }

    #[test]
    fn roster_events_keep_the_envelope_timestamp() {
        for event in [
            Event::BackgroundTasksChanged(wire::BackgroundTasksChanged::default()),
            Event::TaskEvent(wire::TaskEvent {
                edge: wire::TaskEdge::Started as i32,
                ..Default::default()
            }),
        ] {
            let domain = super::super::translate::translate(
                wire::SessionEvent {
                    occurred_at: 1234,
                    event: Some(event),
                    ..Default::default()
                },
                crate::RequestedCliMode::Default,
            );
            assert_eq!(serde_json::to_value(&domain[0]).unwrap()["occurred_at"], 1234);
        }
    }

    #[test]
    fn task_status_values_are_explicit() {
        for (wire, domain) in [
            (wire::TaskStatus::Pending, TaskStatus::Running),
            (wire::TaskStatus::Running, TaskStatus::Running),
            (wire::TaskStatus::Paused, TaskStatus::Paused),
            (wire::TaskStatus::Completed, TaskStatus::Completed),
            (wire::TaskStatus::Failed, TaskStatus::Failed),
            (wire::TaskStatus::Stopped, TaskStatus::Stopped),
            (wire::TaskStatus::Killed, TaskStatus::Stopped),
        ] {
            assert_eq!(task_status(wire as i32), Some(domain));
        }
        assert_eq!(task_status(wire::TaskStatus::Unspecified as i32), None);
        assert_eq!(task_status(999), None);
    }
    #[test]
    fn task_kind_values_are_explicit() {
        for (wire, domain) in [
            (wire::TaskKind::Shell, TaskKind::Shell),
            (wire::TaskKind::Monitor, TaskKind::Monitor),
            (wire::TaskKind::LocalAgent, TaskKind::LocalAgent),
            (wire::TaskKind::Workflow, TaskKind::Workflow),
            (wire::TaskKind::Other, TaskKind::Other),
        ] {
            assert_eq!(task_kind(wire as i32), Some(domain));
        }
        assert_eq!(task_kind(0), None);
        assert_eq!(task_kind(999), None);
    }
    #[test]
    fn task_edge_values_are_explicit() {
        for (wire, domain) in [
            (wire::TaskEdge::Started, TaskEdge::Started),
            (wire::TaskEdge::Progress, TaskEdge::Progress),
            (wire::TaskEdge::Updated, TaskEdge::Updated),
            (wire::TaskEdge::Notified, TaskEdge::Notified),
        ] {
            assert_eq!(task_edge(wire as i32), Some(domain));
        }
        assert_eq!(task_edge(0), None);
        assert_eq!(task_edge(999), None);
        assert!(translate(Event::TaskEvent(wire::TaskEvent::default())).is_empty());
    }
    #[test]
    fn notice_kinds_and_levels_are_explicit() {
        for (wire, expected) in [
            (wire::SessionNoticeKind::ApiRetry, "api_retry"),
            (wire::SessionNoticeKind::RateLimit, "rate_limit"),
            (wire::SessionNoticeKind::ModelFallback, "model_fallback"),
            (wire::SessionNoticeKind::ModelRefusal, "model_refusal"),
            (wire::SessionNoticeKind::Compacting, "compacting"),
            (wire::SessionNoticeKind::CompactBoundary, "compaction"),
            (wire::SessionNoticeKind::Informational, "informational"),
            (wire::SessionNoticeKind::Notification, "notification"),
            (wire::SessionNoticeKind::PeerHold, "peer_hold"),
        ] {
            let event = one(Event::SessionNotice(wire::SessionNotice {
                kind: wire as i32,
                ..Default::default()
            }));
            assert_eq!(serde_json::to_value(event).unwrap()["kind"], expected);
        }
        assert!(translate(Event::SessionNotice(wire::SessionNotice::default())).is_empty());
        for (wire, expected) in [
            (wire::NoticeLevel::Info, NoticeLevel::Info),
            (wire::NoticeLevel::Notice, NoticeLevel::Notice),
            (wire::NoticeLevel::Suggestion, NoticeLevel::Suggestion),
            (wire::NoticeLevel::Warning, NoticeLevel::Warning),
            (wire::NoticeLevel::Unspecified, NoticeLevel::Notice),
        ] {
            assert_eq!(notice_level(wire as i32), expected);
        }
        assert_eq!(notice_level(999), NoticeLevel::Notice);
    }
    #[test]
    fn notice_scalars_keep_every_field_and_decode_enum_words() {
        let value = wire::SessionNotice {
            kind: wire::SessionNoticeKind::ApiRetry as i32,
            level: wire::NoticeLevel::Warning as i32,
            content: Some("retrying".into()),
            attempt: Some(2),
            max_retries: Some(5),
            retry_delay_ms: Some(100),
            error_status: Some(529),
            from_model: Some("old".into()),
            to_model: Some("new".into()),
            pre_tokens: Some(1000),
            post_tokens: Some(100),
            rate_status: Some(wire::RateLimitStatus::AllowedWarning as i32),
            resets_at_ms: Some(200),
            utilization: Some(0.7),
            trigger: Some("auto".into()),
            peer_name: Some("peer".into()),
            hold_state: Some(wire::PeerHoldState::Held as i32),
            error_kind: Some("overloaded".into()),
            fallback_scope: Some("local".into()),
        };
        let event = serde_json::to_value(one(Event::SessionNotice(value.clone()))).unwrap();
        assert_eq!(
            event["scalars"],
            json!({"attempt":2,"max_retries":5,"retry_delay_ms":100,"error_status":529,"from_model":"old","to_model":"new",
            "pre_tokens":1000,"post_tokens":100,"rate_status":"allowed_warning","resets_at_ms":200,"utilization":0.7,"trigger":"auto","peer_name":"peer",
            "hold_state":"held","error_kind":"overloaded","fallback_scope":"local"})
        );
        for (status, expected) in [
            (wire::RateLimitStatus::Allowed, Some("allowed")),
            (wire::RateLimitStatus::AllowedWarning, Some("allowed_warning")),
            (wire::RateLimitStatus::Rejected, Some("rejected")),
            (wire::RateLimitStatus::Unspecified, None),
        ] {
            let e = serde_json::to_value(
                notice(wire::SessionNotice {
                    rate_status: Some(status as i32),
                    ..value.clone()
                })
                .unwrap(),
            )
            .unwrap();
            assert_eq!(e["scalars"]["rate_status"].as_str(), expected);
        }
        for (hold, expected) in [
            (wire::PeerHoldState::Held, Some("held")),
            (wire::PeerHoldState::Released, Some("released")),
            (wire::PeerHoldState::Dropped, Some("dropped")),
            (wire::PeerHoldState::Unspecified, None),
        ] {
            let e = serde_json::to_value(
                notice(wire::SessionNotice {
                    hold_state: Some(hold as i32),
                    ..value.clone()
                })
                .unwrap(),
            )
            .unwrap();
            assert_eq!(e["scalars"]["hold_state"].as_str(), expected);
        }
    }
    #[test]
    fn cli_and_follow_up_starts_are_attributed_in_order() {
        for (origin, expected) in [
            (wire::TurnOrigin::Cli, TurnOrigin::Cli),
            (wire::TurnOrigin::FollowUp, TurnOrigin::FollowUp),
        ] {
            let events = translate(Event::TurnStarted(wire::TurnStarted {
                turn_id: "t".into(),
                origin: origin as i32,
                cause: Some(wire::TurnCause {
                    kind: wire::TurnCauseKind::Scheduled as i32,
                    prompt: Some("wake".into()),
                    ..Default::default()
                }),
            }));
            assert!(
                matches!(&events[..], [Domain::TurnStarted { turn_id }, Domain::TurnAttributed { turn_id: attributed, origin, cause: Some(cause) }]
                if turn_id == "t" && attributed == "t" && origin == &expected && cause.prompt.as_deref() == Some("wake"))
            );
        }
        for origin in [
            wire::TurnOrigin::Client as i32,
            wire::TurnOrigin::Unspecified as i32,
            999,
        ] {
            assert_eq!(
                translate(Event::TurnStarted(wire::TurnStarted {
                    turn_id: "t".into(),
                    origin,
                    ..Default::default()
                })),
                vec![Domain::TurnStarted { turn_id: "t".into() }]
            );
        }
    }
    #[test]
    fn turn_causes_decode_all_kinds_and_keep_their_fields() {
        for (kind, expected) in [
            (wire::TurnCauseKind::Task, TurnCauseKind::Task),
            (wire::TurnCauseKind::Scheduled, TurnCauseKind::Scheduled),
            (wire::TurnCauseKind::Peer, TurnCauseKind::Peer),
            (wire::TurnCauseKind::Other, TurnCauseKind::Other),
            (wire::TurnCauseKind::Unknown, TurnCauseKind::Unknown),
        ] {
            let value = wire::TurnCause {
                kind: kind as i32,
                task_id: Some("b".into()),
                task_kind: Some(wire::TaskKind::LocalAgent as i32),
                task_status: Some(wire::TaskStatus::Killed as i32),
                description: Some("desc".into()),
                summary: Some("summary".into()),
                peer_name: Some("peer".into()),
                text: Some("body".into()),
                wakeup_source: Some("cron".into()),
                scheduled_for_ms: Some(42),
                schedule_reason: Some("due".into()),
                cron_id: Some("c".into()),
                prompt: Some("go".into()),
            };
            let mapped = cause(value.clone()).unwrap();
            assert_eq!(mapped.kind, expected);
            assert_eq!(mapped.task_kind.as_deref(), Some("local_agent"));
            assert_eq!(mapped.task_status.as_deref(), Some("stopped"));
            assert_eq!(mapped.scheduled_for_ms, Some(42));
            assert_eq!(mapped.texts().count(), 11);
            assert!(
                matches!(one(Event::TurnCauseUpdated(wire::TurnCauseUpdated { turn_id:"t".into(), cause:Some(value.clone()) })), Domain::TurnCauseUpdated { cause:Some(c), .. } if c == mapped)
            );
            assert!(
                matches!(one(Event::CliCycleStarted(wire::CliCycleStarted { turn_id:"t".into(), cause:Some(value) })), Domain::CliCycleStarted { cause:Some(c), .. } if c == mapped)
            );
        }
        assert!(cause(wire::TurnCause::default()).is_none());
    }
    #[test]
    fn completion_keeps_origin_count_and_no_result() {
        let event = one(Event::TurnCompleted(wire::TurnCompleted {
            turn_id: "t".into(),
            outcome: wire::TurnOutcome::Failed as i32,
            terminal_reason: Some("no_result".into()),
            origin_kind: Some("task-notification".into()),
            num_turns: Some(3),
            ..Default::default()
        }));
        assert!(
            matches!(event, Domain::TurnCompleted { detail, .. } if detail.origin_kind.as_deref() == Some("task-notification") && detail.num_turns == Some(3) && detail.reason.as_deref() == Some("no_result"))
        );
    }
    #[test]
    fn all_four_child_frames_are_wrapped_and_keep_the_inner_shape() {
        for event in [
            Event::TextDelta(wire::TextDelta {
                turn_id: "t".into(),
                text: "reply".into(),
                parent_tool_use_id: Some("p".into()),
                ..Default::default()
            }),
            Event::ThinkingDelta(wire::ThinkingDelta {
                turn_id: "t".into(),
                text: "thought".into(),
                parent_tool_use_id: Some("p".into()),
                ..Default::default()
            }),
            Event::ToolCallStarted(wire::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: "c".into(),
                name: "Read".into(),
                input_json: "{}".into(),
                parent_tool_use_id: Some("p".into()),
            }),
            Event::ToolCallCompleted(wire::ToolCallCompleted {
                turn_id: "t".into(),
                tool_use_id: "c".into(),
                content_json: "\"ok\"".into(),
                is_error: false,
                parent_tool_use_id: Some("p".into()),
            }),
        ] {
            assert!(
                matches!(one(event), Domain::SubagentFrame { parent_tool_use_id, frame } if parent_tool_use_id == "p" && matches!(*frame, Domain::ContentDelta { .. } | Domain::ToolCallStarted { .. } | Domain::ToolCallCompleted { .. }))
            );
        }
    }
    #[test]
    fn child_message_ids_do_not_split_the_main_message() {
        let mut split = super::super::translate::MessageSplit::new(true);
        for (parent, id) in [(None, "main"), (Some("parent"), "child"), (None, "main")] {
            let event = wire::SessionEvent {
                event: Some(Event::TextDelta(wire::TextDelta {
                    turn_id: "t".into(),
                    message_id: Some(id.into()),
                    parent_tool_use_id: parent.map(str::to_owned),
                    ..Default::default()
                })),
                ..Default::default()
            };
            assert_eq!(split.before(&event), None);
        }
        assert_eq!(split.last_text(), Some("main"));
    }
    #[test]
    fn background_task_and_workflow_fields_all_arrive() {
        let task_value = wire::BackgroundTask {
            task_id: "b".into(),
            run_id: Some("r".into()),
            kind: wire::TaskKind::Workflow as i32,
            task_type: "workflow".into(),
            description: "desc".into(),
            subagent_type: Some("agent".into()),
            parent_task_id: Some("parent".into()),
            tool_use_id: Some("tool".into()),
            ambient: true,
            workflow_name: Some("flow".into()),
        };
        let mapped = background_task(task_value.clone());
        assert!(
            matches!(one(Event::BackgroundTasksChanged(wire::BackgroundTasksChanged { tasks:vec![task_value.clone()] })), Domain::BackgroundTasksChanged { tasks, .. } if tasks == vec![mapped.clone()])
        );
        let progress = wire::WorkflowProgress {
            phases: vec![wire::WorkflowPhase {
                index: 2,
                title: "phase".into(),
                kind: "parallel".into(),
            }],
            agents: vec![wire::WorkflowAgent {
                index: 3,
                label: "agent".into(),
                phase_index: 2,
                state: "running".into(),
                model: Some("model".into()),
                agent_type: Some("type".into()),
                tokens: Some(10),
                tool_calls: Some(2),
                duration_ms: Some(30),
                error: Some("error".into()),
            }],
        };
        let value = wire::TaskEvent {
            edge: wire::TaskEdge::Updated as i32,
            task_id: "b".into(),
            run_id: Some("r".into()),
            task: Some(task_value),
            status: Some(wire::TaskStatus::Paused as i32),
            summary: Some("sum".into()),
            last_tool_name: Some("Read".into()),
            usage: Some(wire::TaskUsage {
                total_tokens: 4,
                tool_uses: 2,
                duration_ms: 30,
            }),
            end_time_ms: Some(42),
            error: Some("error".into()),
            is_backgrounded: Some(true),
            reason: Some("reason".into()),
            output_file: Some("out".into()),
            workflow_progress: Some(progress.clone()),
        };
        assert_eq!(
            serde_json::to_value(one(Event::TaskEvent(value))).unwrap(),
            json!({"type":"task_event","edge":"updated","task_id":"b","run_id":"r","task":mapped,"status":"paused","summary":"sum",
            "last_tool_name":"Read","usage":{"total_tokens":4,"tool_uses":2,"duration_ms":30},"end_time_ms":42,"error":"error","is_backgrounded":true,"reason":"reason","output_file":"out","workflow_progress":workflow(progress)})
        );
    }
    #[test]
    fn reset_and_output_tail_are_translated() {
        assert_eq!(
            one(Event::ConversationReset(wire::ConversationReset {
                new_conversation_id: "new".into(),
                trigger: Some("clear".into())
            })),
            Domain::ConversationReset {
                new_conversation_id: "new".into(),
                trigger: Some("clear".into())
            }
        );
        assert_eq!(
            task_output(wire::GetTaskOutputResponse {
                output: vec![0x80, b'o', b'k'],
                total_bytes: 100,
                truncated: true,
                offset: 97,
            }),
            TaskOutputTail {
                output: "�ok".into(),
                total_bytes: 100,
                truncated: true,
                offset: 97,
            }
        );
    }
    #[test]
    fn a_range_keeps_its_start_says_where_it_is_and_stops_at_the_limit() {
        let range = task_output_range(wire::GetTaskOutputResponse {
            output: vec![b'a'; 10],
            total_bytes: 100,
            truncated: true,
            offset: 40,
        });
        assert_eq!((range.output.len(), range.offset, range.truncated), (10, 40, true));
        let over = task_output_range(wire::GetTaskOutputResponse {
            output: [vec![b'a'; OUTPUT_RANGE_BYTES], vec![b'z'; 5]].concat(),
            total_bytes: 2_000_000,
            truncated: false,
            offset: 0,
        });
        assert_eq!(over.output.len(), OUTPUT_RANGE_BYTES);
        assert!(!over.output.contains('z'), "what goes is the end of what was asked for");
        assert!(over.truncated);
    }

    #[test]
    fn the_applied_settings_read_nothing_from_nothing_and_every_word_is_bounded() {
        assert_eq!(applied_settings(None), crate::AppliedSettings::default());
        let long = "x".repeat(SETTINGS_WORD_CHARS + 5);
        let applied = applied_settings(Some(wire::AppliedSettings {
            model: long.clone(),
            effort: "low".into(),
        }));
        assert_eq!(applied.model.chars().count(), SETTINGS_WORD_CHARS);
        assert_eq!(applied.effort, "low");
    }

    #[test]
    fn the_model_list_keeps_the_clis_order_and_bounds_rows_and_words() {
        let row = |value: &str| wire::ModelOption {
            value: value.into(),
            description: "d".repeat(MODEL_DESCRIPTION_CHARS + 9),
            supported_effort_levels: vec!["low".into(); EFFORT_ROWS + 3],
            supports_auto_mode: true,
            ..Default::default()
        };
        let list = model_list(wire::ListModelsResponse {
            models: std::iter::repeat_with(|| row("x")).take(MODEL_ROWS + 2).collect(),
            applied: None,
        });
        assert_eq!(list.models.len(), MODEL_ROWS);
        assert_eq!(list.models[0].description.chars().count(), MODEL_DESCRIPTION_CHARS);
        assert_eq!(list.models[0].supported_effort_levels.len(), EFFORT_ROWS);
        let ordered = model_list(wire::ListModelsResponse {
            models: vec![row("b"), row("a")],
            applied: None,
        });
        assert_eq!(ordered.models[0].value, "b");
        assert!(ordered.models[0].supports_auto_mode);
    }

    #[test]
    fn output_tail_keeps_its_last_8192_bytes_and_says_it_was_cut() {
        let at: Vec<u8> = (0..8192).map(|i| b'a' + (i % 26) as u8).collect();
        let tail = task_output(wire::GetTaskOutputResponse {
            output: at.clone(),
            total_bytes: 8192,
            truncated: false,
            offset: 0,
        });
        assert_eq!(tail.output.as_bytes(), &at[..]);
        assert!(!tail.truncated);
        let mut over = vec![b'X'];
        over.extend_from_slice(&at);
        let tail = task_output(wire::GetTaskOutputResponse {
            output: over,
            total_bytes: 9000,
            truncated: false,
            offset: 807,
        });
        assert_eq!(tail.output.as_bytes(), &at[..], "the first byte goes, the end stays");
        assert_eq!(tail.offset, 808, "the offset moves with the cut");
        assert_eq!(tail.total_bytes, 9000);
        assert!(tail.truncated);
    }
    #[test]
    fn stop_resolve_and_interaction_state_responses_are_mapped() {
        task_stopped(wire::StopTaskResponse {});
        for (wire, expected) in [
            (wire::InteractionOutcome::Answered, Some(QuestionOutcome::Answered)),
            (wire::InteractionOutcome::Declined, Some(QuestionOutcome::Declined)),
            (
                wire::InteractionOutcome::HostRefused,
                Some(QuestionOutcome::HostRefused),
            ),
            (wire::InteractionOutcome::Unspecified, None),
        ] {
            assert_eq!(
                resolve_outcome(wire::ResolveInteractionResponse { outcome: wire as i32 }),
                expected
            );
        }
        let mut too_long = question();
        edit_question(&mut too_long).header = "x".repeat(257);
        let state = interaction_state(wire::GetInteractionStateResponse {
            through_sequence: 41,
            pending: vec![question(), too_long],
        });
        assert_eq!(state.through_sequence, 41);
        assert_eq!(
            state.pending,
            vec![
                requested(question()),
                requested({
                    let mut q = question();
                    edit_question(&mut q).header = "x".repeat(257);
                    q
                })
            ]
        );
        assert!(
            matches!(&state.pending[1], Domain::QuestionAsked { unsupported_reason: Some(_), questions, .. } if questions.is_empty()),
            "a pending question over a bound is refused the same way a live one is"
        );
    }
    #[test]
    fn a_task_of_unknown_kind_stays_in_the_roster_as_other() {
        for kind in [wire::TaskKind::Unspecified as i32, 999] {
            let Domain::BackgroundTasksChanged { tasks, .. } = roster(
                wire::BackgroundTasksChanged {
                    tasks: vec![wire::BackgroundTask {
                        task_id: "b".into(),
                        kind,
                        ..Default::default()
                    }],
                },
                None,
            ) else {
                panic!("roster")
            };
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].kind, TaskKind::Other);
        }
    }
    #[test]
    fn a_settlement_with_an_unreadable_outcome_still_settles_as_a_failure() {
        for outcome in [wire::InteractionOutcome::Unspecified as i32, 999] {
            let settled = resolved(wire::InteractionResolved {
                interaction_id: "i1".into(),
                outcome,
                answers: Vec::new(),
                reason: None,
                ..Default::default()
            });
            assert!(
                matches!(
                    settled,
                    Some(Domain::QuestionSettled {
                        outcome: QuestionOutcome::ProviderFailed,
                        ..
                    })
                ),
                "an unreadable outcome is never taken for an answer, and never leaves the question open"
            );
        }
    }
    #[test]
    fn question_requested_is_lossless_and_question_outcomes_are_explicit() {
        assert!(
            matches!(one(Event::InteractionRequested(question())), Domain::QuestionAsked { interaction_id, questions, unsupported_reason:None, .. } if interaction_id == "i1" && questions[0].options[0].preview.as_deref() == Some("**preview**"))
        );
        for (wire, expected) in [
            (wire::InteractionOutcome::Answered, QuestionOutcome::Answered),
            (wire::InteractionOutcome::Declined, QuestionOutcome::Declined),
            (wire::InteractionOutcome::ChatRequested, QuestionOutcome::ChatRequested),
            (wire::InteractionOutcome::Interrupted, QuestionOutcome::Interrupted),
            (wire::InteractionOutcome::Expired, QuestionOutcome::Expired),
            (wire::InteractionOutcome::SessionClosed, QuestionOutcome::SessionClosed),
            (
                wire::InteractionOutcome::ProviderFailed,
                QuestionOutcome::ProviderFailed,
            ),
            (wire::InteractionOutcome::HostRefused, QuestionOutcome::HostRefused),
        ] {
            assert_eq!(question_outcome(wire as i32), Some(expected));
            let event = one(Event::InteractionResolved(wire::InteractionResolved {
                interaction_id: "i1".into(),
                outcome: wire as i32,
                answers: vec![wire::ShownAnswer {
                    question_index: 0,
                    answer: "A".into(),
                    notes: Some("note".into()),
                }],
                reason: Some("reason".into()),
                ..Default::default()
            }));
            assert!(
                matches!(event, Domain::QuestionSettled { outcome,answers,reason,.. } if outcome == expected && answers[0].answer == "A" && reason.as_deref() == Some("reason"))
            );
        }
        assert_eq!(question_outcome(0), None);
    }
    #[test]
    fn every_turn_string_bound_keeps_the_limit_and_marks_one_over() {
        for limit in [64, 128, 500, 1024, 2000] {
            let at = "𐀀".repeat(limit);
            assert_eq!(bound(&at, limit), at);
            let over = bound(&(at.clone() + "x"), limit);
            assert_eq!(over.chars().count(), limit);
            assert!(over.ends_with('…'));
        }
    }
    #[test]
    fn every_question_text_bound_refuses_instead_of_cutting() {
        for (field, max) in [
            ("question", 4000),
            ("header", 256),
            ("label", 1000),
            ("description", 4000),
            ("preview", 16384),
        ] {
            for extra in [0, 1] {
                let mut value = question();
                let q = edit_question(&mut value);
                let text = "x".repeat(max + extra);
                match field {
                    "question" => q.question = text.clone(),
                    "header" => q.header = text.clone(),
                    "label" => q.options[0].label = text.clone(),
                    "description" => q.options[0].description = text.clone(),
                    "preview" => q.options[0].preview = Some(text.clone()),
                    _ => unreachable!(),
                }
                let event = one(Event::InteractionRequested(value));
                let Domain::QuestionAsked {
                    questions,
                    unsupported_reason,
                    ..
                } = event
                else {
                    panic!("question")
                };
                assert_eq!(unsupported_reason.is_some(), extra == 1, "{field}");
                if extra == 1 {
                    assert!(questions.is_empty());
                } else {
                    assert!(serde_json::to_string(&questions).unwrap().contains(&text));
                }
            }
        }
        let mut value = question();
        edit_question(&mut value).options[0].label = "𐀀".repeat(501);
        assert!(
            matches!(requested(value),Domain::QuestionAsked { unsupported_reason:Some(_),questions,.. } if questions.is_empty())
        );
    }
    #[test]
    fn invalid_question_counts_duplicates_identifiers_and_totals_are_refused() {
        for mode in 0..8 {
            let mut value = question();
            let Some(wire::interaction_requested::Payload::Questions(set)) = value.payload.as_mut() else {
                panic!("set")
            };
            match mode {
                0 => set.questions.clear(),
                1 => set.questions = vec![set.questions[0].clone(); 5],
                2 => set.questions[0].options.truncate(1),
                3 => set.questions[0].options = vec![set.questions[0].options[0].clone(); 5],
                4 => set.questions.push(set.questions[0].clone()),
                5 => set.questions[0].options[1].label = "A".into(),
                6 => value.interaction_id = "x".repeat(129),
                7 => {
                    set.questions = (0..4)
                        .map(|i| {
                            let mut q = set.questions[0].clone();
                            q.question = i.to_string();
                            q.options = (0..4)
                                .map(|j| wire::QuestionOption {
                                    label: j.to_string(),
                                    description: "x".repeat(4000),
                                    preview: Some("x".repeat(16384)),
                                })
                                .collect();
                            q
                        })
                        .collect();
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(requested(value),Domain::QuestionAsked { unsupported_reason:Some(_),questions,.. } if questions.is_empty()),
                "{mode}"
            );
        }
    }
    #[test]
    fn a_plan_approval_and_plan_outcomes_are_not_read_as_questions() {
        let mut value = question();
        value.kind = wire::InteractionKind::PlanApproval as i32;
        assert!(matches!(
            requested(value),
            Domain::QuestionAsked { unsupported_reason: Some(reason), questions, .. }
                if questions.is_empty() && reason == "Unsupported interaction kind"
        ));
        let mut value = question();
        value.payload = Some(wire::interaction_requested::Payload::Plan(wire::PlanProposal::default()));
        assert!(matches!(
            requested(value),
            Domain::QuestionAsked { unsupported_reason: Some(_), questions, .. } if questions.is_empty()
        ));
        for outcome in [
            wire::InteractionOutcome::Approved,
            wire::InteractionOutcome::KeptPlanning,
        ] {
            assert_eq!(question_outcome(outcome as i32), None);
        }
    }
    #[test]
    fn task_and_workflow_count_and_string_bounds_are_rechecked() {
        let task_value = wire::BackgroundTask {
            task_id: "x".repeat(129),
            run_id: Some("x".repeat(129)),
            kind: wire::TaskKind::Shell as i32,
            task_type: "x".repeat(65),
            description: "x".repeat(501),
            subagent_type: Some("x".repeat(129)),
            parent_task_id: Some("x".repeat(129)),
            tool_use_id: Some("x".repeat(129)),
            workflow_name: Some("x".repeat(129)),
            ..Default::default()
        };
        let mapped = serde_json::to_value(background_task(task_value.clone())).unwrap();
        for (key, limit) in [
            ("task_id", 128),
            ("run_id", 128),
            ("task_type", 64),
            ("description", 500),
            ("subagent_type", 128),
            ("parent_task_id", 128),
            ("tool_use_id", 128),
            ("workflow_name", 128),
        ] {
            assert_eq!(mapped[key].as_str().unwrap().chars().count(), limit);
            assert!(mapped[key].as_str().unwrap().ends_with('…'));
        }
        for count in [256, 257] {
            let Domain::BackgroundTasksChanged { tasks, .. } = roster(
                wire::BackgroundTasksChanged {
                    tasks: vec![task_value.clone(); count],
                },
                None,
            ) else {
                panic!("roster")
            };
            assert_eq!(tasks.len(), 256);
        }
        for (phases, agents) in [(64, 256), (65, 257)] {
            let progress = workflow(wire::WorkflowProgress {
                phases: vec![
                    wire::WorkflowPhase {
                        title: "x".repeat(129),
                        kind: "x".repeat(129),
                        ..Default::default()
                    };
                    phases
                ],
                agents: vec![
                    wire::WorkflowAgent {
                        label: "x".repeat(129),
                        state: "x".repeat(129),
                        model: Some("x".repeat(129)),
                        agent_type: Some("x".repeat(129)),
                        error: Some("x".repeat(501)),
                        ..Default::default()
                    };
                    agents
                ],
            });
            assert_eq!(progress.phases.len(), 64);
            assert_eq!(progress.agents.len(), 256);
            assert_eq!(progress.phases[0].title.chars().count(), 128);
            assert_eq!(progress.agents[0].error.as_ref().unwrap().chars().count(), 500);
        }
    }
    fn assert_bound(value: &Value, key: &str, limit: usize, extra: usize) {
        let text = value[key].as_str().unwrap();
        assert_eq!(text.chars().count(), limit, "{key}");
        assert_eq!(text.ends_with('…'), extra == 1, "{key}");
    }

    #[test]
    fn cause_bounds_are_checked_at_the_limit_and_one_over() {
        for extra in [0, 1] {
            let text = |limit| Some("界".repeat(limit + extra));
            let value = wire::TurnCause {
                kind: wire::TurnCauseKind::Scheduled as i32,
                task_id: text(128),
                description: text(500),
                summary: text(2000),
                peer_name: text(128),
                text: text(2000),
                wakeup_source: text(64),
                schedule_reason: text(500),
                cron_id: text(128),
                prompt: text(2000),
                ..Default::default()
            };
            let value = serde_json::to_value(cause(value).unwrap()).unwrap();
            for (key, limit) in [
                ("task_id", 128),
                ("description", 500),
                ("summary", 2000),
                ("peer_name", 128),
                ("text", 2000),
                ("wakeup_source", 64),
                ("schedule_reason", 500),
                ("cron_id", 128),
                ("prompt", 2000),
            ] {
                assert_bound(&value, key, limit, extra);
            }
        }
    }
    #[test]
    fn task_event_bounds_are_checked_at_the_limit_and_one_over() {
        for extra in [0, 1] {
            let text = |limit| Some("界".repeat(limit + extra));
            let event = one(Event::TaskEvent(wire::TaskEvent {
                edge: wire::TaskEdge::Progress as i32,
                task_id: text(128).unwrap(),
                run_id: text(128),
                summary: text(2000),
                last_tool_name: text(64),
                error: text(2000),
                reason: text(2000),
                output_file: text(1024),
                ..Default::default()
            }));
            let value = serde_json::to_value(event).unwrap();
            for (key, limit) in [
                ("task_id", 128),
                ("run_id", 128),
                ("summary", 2000),
                ("last_tool_name", 64),
                ("error", 2000),
                ("reason", 2000),
                ("output_file", 1024),
            ] {
                assert_bound(&value, key, limit, extra);
            }
        }
    }
    #[test]
    fn notice_bounds_are_checked_at_the_limit_and_one_over() {
        for extra in [0, 1] {
            let text = |limit| Some("界".repeat(limit + extra));
            let value = serde_json::to_value(one(Event::SessionNotice(wire::SessionNotice {
                kind: wire::SessionNoticeKind::ModelFallback as i32,
                content: text(2000),
                from_model: text(128),
                to_model: text(128),
                trigger: text(64),
                peer_name: text(128),
                error_kind: text(64),
                fallback_scope: text(64),
                ..Default::default()
            })))
            .unwrap();
            assert_bound(&value, "content", 2000, extra);
            for (key, limit) in [
                ("from_model", 128),
                ("to_model", 128),
                ("trigger", 64),
                ("peer_name", 128),
                ("error_kind", 64),
                ("fallback_scope", 64),
            ] {
                assert_bound(&value["scalars"], key, limit, extra);
            }
        }
    }
    #[test]
    fn existing_arm_ids_text_and_new_completion_fields_are_bounded() {
        for extra in [0, 1] {
            let text = |limit| "界".repeat(limit + extra);
            // A whole message the sidecar did not stream arrives as one delta, so reply and thinking
            // text is never cut, however long.
            let long = "界".repeat(20_000);
            for event in [
                Event::TextDelta(wire::TextDelta {
                    turn_id: text(128),
                    text: long.clone(),
                    ..Default::default()
                }),
                Event::ThinkingDelta(wire::ThinkingDelta {
                    turn_id: text(128),
                    text: long.clone(),
                    ..Default::default()
                }),
            ] {
                let value = serde_json::to_value(one(event)).unwrap();
                assert_bound(&value, "turn_id", 128, extra);
                assert_eq!(value["text"].as_str(), Some(long.as_str()));
            }
            let value = serde_json::to_value(one(Event::ToolCallStarted(wire::ToolCallStarted {
                turn_id: text(128),
                tool_use_id: text(128),
                name: text(128),
                input_json: "{}".into(),
                parent_tool_use_id: Some(text(128)),
            })))
            .unwrap();
            assert_bound(&value, "parent_tool_use_id", 128, extra);
            for key in ["turn_id", "tool_use_id", "name"] {
                assert_bound(&value["frame"], key, 128, extra);
            }
            let value = serde_json::to_value(one(Event::ToolCallCompleted(wire::ToolCallCompleted {
                turn_id: text(128),
                tool_use_id: text(128),
                content_json: "[]".into(),
                parent_tool_use_id: Some(text(128)),
                ..Default::default()
            })))
            .unwrap();
            assert_bound(&value, "parent_tool_use_id", 128, extra);
            for key in ["turn_id", "tool_use_id"] {
                assert_bound(&value["frame"], key, 128, extra);
            }
            let value = serde_json::to_value(one(Event::TurnCompleted(wire::TurnCompleted {
                turn_id: text(128),
                origin_kind: Some(text(64)),
                terminal_reason: Some(text(64)),
                ..Default::default()
            })))
            .unwrap();
            assert_bound(&value, "turn_id", 128, extra);
            assert_bound(&value["detail"], "origin_kind", 64, extra);
            assert_bound(&value["detail"], "reason", 64, extra);
            let value = serde_json::to_value(one(Event::ConversationReset(wire::ConversationReset {
                new_conversation_id: text(128),
                trigger: Some(text(64)),
            })))
            .unwrap();
            assert_bound(&value, "new_conversation_id", 128, extra);
            assert_bound(&value, "trigger", 64, extra);
        }
    }
    #[test]
    fn an_over_long_id_is_cut_the_same_on_every_event_that_names_it() {
        let turn = "t".repeat(200);
        let call = "c".repeat(200);
        let cut_turn = bound(&turn, 128);
        let cut_call = bound(&call, 128);
        let started = one(Event::ToolCallStarted(wire::ToolCallStarted {
            turn_id: turn.clone(),
            tool_use_id: call.clone(),
            name: "Bash".into(),
            input_json: "{}".into(),
            parent_tool_use_id: None,
        }));
        assert!(
            matches!(&started, Domain::ToolCallStarted { turn_id, tool_use_id, .. } if *turn_id == cut_turn && *tool_use_id == cut_call)
        );
        let card = one(Event::PermissionRequested(wire::PermissionRequested {
            permission_id: "p".into(),
            tool_use_id: call.clone(),
            tool_name: "Bash".into(),
            input_json: "{}".into(),
            ..Default::default()
        }));
        assert!(
            matches!(&card, Domain::PermissionRequested { tool_use_id: Some(id), permission_id, .. } if *id == cut_call && permission_id == "p"),
            "the card names the call its ToolCallStarted named, and the permission id it answers is never cut"
        );
        let denied = one(Event::PermissionDenied(wire::PermissionDenied {
            tool_use_id: call.clone(),
            ..Default::default()
        }));
        assert!(matches!(&denied, Domain::PermissionDenied { tool_use_id: Some(id), .. } if *id == cut_call));
        let delivered = one(Event::UserMessageDelivered(wire::UserMessageDelivered {
            client_message_id: "m".into(),
            turn_id: turn.clone(),
            ..Default::default()
        }));
        assert!(matches!(&delivered, Domain::UserMessageDelivered { turn_id, .. } if *turn_id == cut_turn));
        let mut split = super::super::translate::MessageSplit::new(true);
        let mut boundary = None;
        for id in ["m1", "m2"] {
            boundary = split.before(&wire::SessionEvent {
                event: Some(Event::TextDelta(wire::TextDelta {
                    turn_id: turn.clone(),
                    message_id: Some(id.into()),
                    ..Default::default()
                })),
                ..Default::default()
            });
        }
        assert_eq!(boundary, Some(Domain::AssistantMessageBoundary { turn_id: cut_turn }));
    }
    #[test]
    fn question_answer_and_reason_bounds_are_rechecked() {
        for (field, limit) in [("answer", 32768), ("notes", 8192), ("reason", 2000)] {
            for extra in [0, 1] {
                let text = "界".repeat(limit + extra);
                let mut value = wire::InteractionResolved {
                    interaction_id: "i".into(),
                    outcome: wire::InteractionOutcome::Answered as i32,
                    answers: vec![wire::ShownAnswer {
                        question_index: 0,
                        answer: "A".into(),
                        notes: None,
                    }],
                    reason: None,
                    ..Default::default()
                };
                match field {
                    "answer" => value.answers[0].answer = text.clone(),
                    "notes" => value.answers[0].notes = Some(text.clone()),
                    "reason" => value.reason = Some(text.clone()),
                    _ => unreachable!(),
                }
                let Domain::QuestionSettled {
                    outcome,
                    answers,
                    reason,
                    ..
                } = resolved(value).unwrap()
                else {
                    panic!("settled")
                };
                if field == "reason" {
                    assert_eq!(outcome, QuestionOutcome::Answered);
                    assert_eq!(reason.as_ref().unwrap().chars().count(), limit);
                    assert_eq!(reason.unwrap().ends_with('…'), extra == 1);
                } else if extra == 1 {
                    assert_eq!(outcome, QuestionOutcome::HostRefused);
                    assert!(answers.is_empty());
                } else {
                    assert_eq!(outcome, QuestionOutcome::Answered);
                    assert!(serde_json::to_string(&answers).unwrap().contains(&text));
                }
            }
        }
    }
    #[test]
    fn serialized_question_set_accepts_the_byte_limit_and_refuses_one_over() {
        let mut set: Vec<wire::Question> = (0..4)
            .map(|i| wire::Question {
                question: i.to_string(),
                header: String::new(),
                multi_select: false,
                options: (0..4)
                    .map(|j| wire::QuestionOption {
                        label: j.to_string(),
                        description: String::new(),
                        preview: Some(String::new()),
                    })
                    .collect(),
            })
            .collect();
        // All text here is ASCII and unescaped; the constant counts only the JSON syntax and labels.
        let empty_option = r#"{"label":"0","description":"","preview":""}"#;
        let empty_question = format!(
            r#"{{"question":"0","header":"","multiSelect":false,"options":[{}]}}"#,
            vec![empty_option; 4].join(",")
        );
        let overhead = format!("[{}]", vec![empty_question; 4].join(",")).len();
        let mut remaining = 256 * 1024 - overhead;
        for q in &mut set {
            for o in &mut q.options {
                let take = remaining.min(16384);
                o.preview = Some("x".repeat(take));
                remaining -= take;
            }
        }
        assert_eq!(remaining, 0);
        assert!(valid_questions(&set));
        set[3].options[3].preview.as_mut().unwrap().push('x');
        assert!(!valid_questions(&set));
    }
    #[test]
    fn answered_set_aggregate_length_is_checked_in_utf16_units() {
        for extra in [0, 1] {
            let value = wire::InteractionResolved {
                interaction_id: "i".into(),
                outcome: wire::InteractionOutcome::Answered as i32,
                answers: vec![
                    wire::ShownAnswer {
                        question_index: 0,
                        answer: "𐀀".repeat(8192),
                        notes: None,
                    },
                    wire::ShownAnswer {
                        question_index: 1,
                        answer: "x".repeat(16384 + extra),
                        notes: None,
                    },
                ],
                reason: None,
                ..Default::default()
            };
            let Domain::QuestionSettled { outcome, answers, .. } = resolved(value).unwrap() else {
                panic!("settled")
            };
            assert_eq!(
                outcome,
                if extra == 0 {
                    QuestionOutcome::Answered
                } else {
                    QuestionOutcome::HostRefused
                }
            );
            assert_eq!(answers.len(), if extra == 0 { 2 } else { 0 });
        }
    }
}
