use super::{
    AgentDomainEvent, AgentSessionProjection, BackgroundTask, NoticeLevel, NoticeRecord, SessionNoticeKind, TaskEdge,
    TaskKind, TaskStatus, TaskUsage, WorkflowProgress,
};
use serde::{Deserialize, Serialize};

/// A process's task roster, including rows awaiting an outcome after leaving the level.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRoster {
    pub rows: Vec<BackgroundTaskRecord>,
    /// Set when the CLI process ended: the next level is the empty one a new query begins with, and
    /// it clears the roster instead of leaving rows to wait for an outcome that cannot come.
    pub query_start: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskRecord {
    #[serde(flatten)]
    pub task: BackgroundTask,
    pub status: TaskStatus,
    pub live: bool,
    /// The arrival sequence is the stable clock when the provider supplies no start timestamp.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub finished_seq: Option<u64>,
    pub summary: Option<String>,
    pub last_tool_name: Option<String>,
    pub usage: Option<TaskUsage>,
    pub error: Option<String>,
    pub is_backgrounded: Option<bool>,
    pub reason: Option<String>,
    pub output_file: Option<String>,
    pub workflow_progress: Option<WorkflowProgress>,
}

fn terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Ended
    )
}

impl BackgroundTaskRecord {
    fn new(task: BackgroundTask, at: i64) -> Self {
        Self {
            task,
            status: TaskStatus::Running,
            live: false,
            started_at: at,
            ended_at: None,
            finished_seq: None,
            summary: None,
            last_tool_name: None,
            usage: None,
            error: None,
            is_backgrounded: None,
            reason: None,
            output_file: None,
            workflow_progress: None,
        }
    }

    fn end(&mut self, status: TaskStatus, seq: u64, at: i64) {
        // A second terminal edge (the notification after an update) restates the outcome; the moment
        // the task ended is the first one recorded, not the later delivery time.
        if !terminal(self.status) || self.ended_at.is_none() {
            self.ended_at = Some(at);
        }
        self.status = status;
        self.live = false;
        self.finished_seq = Some(seq);
    }
}

impl TaskRoster {
    pub fn running(&self) -> u32 {
        self.rows.iter().filter(|row| row.live && !row.task.ambient).count() as u32
    }

    fn trim(&mut self) {
        // Stable ordering preserves arrival order when several tasks settle in one event.
        self.rows
            .sort_by(|a, b| b.finished_seq.cmp(&a.finished_seq).then_with(|| b.live.cmp(&a.live)));
        let (mut pending, mut finished) = (0, 0);
        self.rows.retain(|row| {
            if terminal(row.status) {
                finished += 1;
                finished <= 20
            } else {
                pending += 1;
                pending <= 256
            }
        });
    }

    fn row(
        &mut self,
        task_id: &str,
        run_id: Option<&str>,
        task: Option<&BackgroundTask>,
        at: i64,
        provisional: bool,
    ) -> Option<usize> {
        if let Some(index) = self.rows.iter().position(|row| row.task.task_id == task_id) {
            let row = &mut self.rows[index];
            if let (Some(incoming), Some(current)) = (run_id, row.task.run_id.as_deref()) {
                if incoming < current {
                    return None;
                }
                if incoming > current {
                    let mut next = task.cloned().unwrap_or_else(|| row.task.clone());
                    next.run_id = Some(incoming.to_owned());
                    *row = BackgroundTaskRecord::new(next, at);
                }
            }
            if let Some(task) = task {
                // A provisional shell kind must not undo the tool join from STARTED.
                let joined = provisional && row.task.kind != TaskKind::Shell && task.kind == TaskKind::Shell;
                let old_kind = row.task.kind;
                let old_tool = row.task.tool_use_id.clone();
                let previous = row.task.clone();
                row.task = task.clone();
                row.task.run_id = row.task.run_id.take().or(previous.run_id);
                row.task.tool_use_id = row.task.tool_use_id.take().or(previous.tool_use_id);
                row.task.workflow_name = row.task.workflow_name.take().or(previous.workflow_name);
                row.task.subagent_type = row.task.subagent_type.take().or(previous.subagent_type);
                row.task.parent_task_id = row.task.parent_task_id.take().or(previous.parent_task_id);
                if row.task.description.is_empty() {
                    row.task.description = previous.description;
                }
                if row.task.task_type.is_empty() {
                    row.task.task_type = previous.task_type;
                }
                if joined {
                    row.task.kind = old_kind;
                    row.task.tool_use_id = old_tool.or(row.task.tool_use_id.take());
                }
            }
            row.task.task_id = task_id.to_owned();
            if let Some(run) = run_id {
                row.task.run_id = Some(run.to_owned());
            }
            return Some(index);
        }
        let mut task = task.cloned().unwrap_or_else(|| BackgroundTask {
            task_id: task_id.to_owned(),
            run_id: run_id.map(str::to_owned),
            kind: TaskKind::Other,
            task_type: String::new(),
            description: String::new(),
            subagent_type: None,
            parent_task_id: None,
            tool_use_id: None,
            ambient: false,
            workflow_name: None,
        });
        task.task_id = task_id.to_owned();
        if let Some(run) = run_id {
            task.run_id = Some(run.to_owned());
        }
        self.rows.push(BackgroundTaskRecord::new(task, at));
        Some(self.rows.len() - 1)
    }

    fn update(&mut self, event: &AgentDomainEvent, seq: u64) -> Option<NoticeRecord> {
        let occurred_at = match event {
            AgentDomainEvent::BackgroundTasksChanged { occurred_at, .. }
            | AgentDomainEvent::TaskEvent { occurred_at, .. } => *occurred_at,
            _ => None,
        };
        let at = occurred_at.unwrap_or(seq as i64);
        let mut notice = None;
        match event {
            AgentDomainEvent::BackgroundTasksChanged { tasks, .. } => {
                if tasks.is_empty() && self.query_start {
                    self.rows.clear();
                }
                self.query_start = false;
                let previous_live: Vec<_> = self.rows.iter().map(|r| (r.task.task_id.clone(), r.live)).collect();
                for row in &mut self.rows {
                    row.live = false;
                }
                for task in tasks.iter().take(256) {
                    if let Some(index) = self.row(&task.task_id, task.run_id.as_deref(), Some(task), at, true) {
                        let row = &mut self.rows[index];
                        // The level owns membership; pause and a pending stop survive repeated levels.
                        row.live = true;
                        if !matches!(row.status, TaskStatus::Stopping | TaskStatus::Paused) {
                            row.status = TaskStatus::Running;
                        }
                        row.ended_at = None;
                        row.finished_seq = None;
                    } else if let Some(row) = self.rows.iter_mut().find(|r| r.task.task_id == task.task_id) {
                        row.live = previous_live
                            .iter()
                            .find(|(id, _)| *id == task.task_id)
                            .is_some_and(|(_, live)| *live);
                    }
                }
                for row in &mut self.rows {
                    if !row.live && row.status == TaskStatus::Stopping {
                        row.end(TaskStatus::Stopped, seq, at);
                    }
                }
            }
            AgentDomainEvent::TaskEvent {
                edge,
                task_id,
                run_id,
                task,
                status,
                summary,
                last_tool_name,
                usage,
                end_time_ms,
                error,
                is_backgrounded,
                reason,
                output_file,
                workflow_progress,
                ..
            } => {
                if task.is_none()
                    && status.is_none()
                    && summary.is_none()
                    && last_tool_name.is_none()
                    && usage.is_none()
                    && error.is_none()
                    && is_backgrounded.is_none()
                    && reason.is_none()
                    && output_file.is_none()
                    && workflow_progress.is_none()
                    && end_time_ms.is_none()
                    && !self.rows.iter().any(|row| row.task.task_id == *task_id)
                {
                    return None;
                }
                let run = run_id
                    .as_deref()
                    .or_else(|| task.as_ref().and_then(|t| t.run_id.as_deref()));
                let index = self.row(task_id, run, task.as_ref(), at, false)?;
                let row = &mut self.rows[index];
                if let Some(v) = summary {
                    row.summary = Some(v.clone());
                }
                if let Some(v) = last_tool_name {
                    row.last_tool_name = Some(v.clone());
                }
                if let Some(v) = usage {
                    row.usage = Some(v.clone());
                }
                if let Some(v) = error {
                    row.error = Some(v.clone());
                }
                if let Some(v) = is_backgrounded {
                    row.is_backgrounded = Some(*v);
                }
                if let Some(v) = reason {
                    row.reason = Some(v.clone());
                }
                if let Some(v) = output_file {
                    row.output_file = Some(v.clone());
                }
                if *edge == TaskEdge::Progress {
                    row.workflow_progress = workflow_progress.clone();
                }
                if *edge == TaskEdge::Started && occurred_at.is_some() {
                    row.started_at = at;
                }
                if *edge == TaskEdge::Started && *is_backgrounded == Some(true) && !terminal(row.status) {
                    row.live = true;
                }
                if let Some(status) = status {
                    if terminal(*status) {
                        row.end(*status, seq, at);
                    } else if !terminal(row.status) && row.status != TaskStatus::Stopping {
                        row.status = *status;
                    }
                }
                if reason.as_deref() == Some("worker_restart") {
                    row.end(TaskStatus::Stopped, seq, at);
                    row.summary = Some("stopped (Claude Code restarted)".into());
                }
                // The CLI's own end time beats arrival time, but only for a row that has ended: a running
                // task carries no end.
                if let Some(end) = end_time_ms {
                    if terminal(row.status) {
                        row.ended_at = Some(*end);
                    }
                }
                if *edge == TaskEdge::Notified
                    && status.is_some_and(|s| terminal(s) || s == TaskStatus::Running)
                    && !row.task.ambient
                {
                    notice = Some(NoticeRecord {
                        seq,
                        kind: SessionNoticeKind::Task,
                        level: NoticeLevel::Notice,
                        content: summary.clone(),
                        scalars: Default::default(),
                        task_id: Some(task_id.clone()),
                        tool_use_id: row.task.tool_use_id.clone(),
                        task_status: status.map(|s| serde_json::to_value(s).unwrap().as_str().unwrap().to_owned()),
                    });
                }
            }
            AgentDomainEvent::TurnCompleted { .. } => {
                for row in &mut self.rows {
                    if !row.live && !terminal(row.status) {
                        row.end(TaskStatus::Ended, seq, at);
                    }
                }
            }
            // `SessionOpened` is not here: the CLI re-sends its init on every turn of one process, so it
            // says nothing about the tasks that process runs.
            AgentDomainEvent::SessionClosed { .. }
            | AgentDomainEvent::SessionUnavailable { .. }
            | AgentDomainEvent::ResumeOutcome { .. } => {
                for row in &mut self.rows {
                    if !terminal(row.status) {
                        row.end(TaskStatus::Ended, seq, at);
                        row.summary = Some("stopped with the session".into());
                    }
                }
                // The process is gone, so the next level is the empty one a new query begins with. A
                // resume's outcome arrives after that level, so it does not arm the clear: the next empty
                // level then is an ordinary one inside the new process.
                if !matches!(event, AgentDomainEvent::ResumeOutcome { .. }) {
                    self.query_start = !self.rows.is_empty();
                }
            }
            _ => {}
        }
        self.trim();
        notice
    }
}

pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("roster::fold");
    if let Some(notice) = state.background_tasks.update(event, state.last_revision) {
        state.assistant_message_open = false;
        state.thinking_open = false;
        state.notices.push(notice);
    }
}

pub(super) fn snapshot_json(state: &AgentSessionProjection, snapshot: &mut serde_json::Value) {
    #[cfg(test)]
    super::count_hook("roster::snapshot_json");
    if !state.background_tasks.rows.is_empty() || state.background_tasks.query_start {
        snapshot["backgroundTasks"] = serde_json::to_value(&state.background_tasks).unwrap();
    }
}

pub(super) fn on_turn_completed(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("roster::on_turn_completed");
    state.background_tasks.update(event, state.last_revision);
}

pub(super) fn on_session_end(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("roster::on_session_end");
    state.background_tasks.update(event, state.last_revision);
}

pub(super) fn derived_after(state: &AgentSessionProjection, event: &AgentDomainEvent) -> Option<AgentDomainEvent> {
    #[cfg(test)]
    super::count_hook("roster::derived_after");
    if !matches!(
        event,
        AgentDomainEvent::BackgroundTasksChanged { .. }
            | AgentDomainEvent::TaskEvent { .. }
            | AgentDomainEvent::SessionClosed { .. }
            | AgentDomainEvent::SessionUnavailable { .. }
            | AgentDomainEvent::ResumeOutcome { .. }
    ) {
        return None;
    }
    let mut next = state.background_tasks.clone();
    next.update(event, state.last_revision);
    let running = next.running();
    (running != state.background_tasks.running()).then_some(AgentDomainEvent::BackgroundCalls { running })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::claude_sidecar::translate_322;
    use claude_runtime_protocol::v1 as wire;

    fn wire_task(task: &BackgroundTask) -> wire::BackgroundTask {
        wire::BackgroundTask {
            task_id: task.task_id.clone(),
            run_id: task.run_id.clone(),
            kind: match task.kind {
                TaskKind::Shell => wire::TaskKind::Shell,
                TaskKind::Monitor => wire::TaskKind::Monitor,
                TaskKind::LocalAgent => wire::TaskKind::LocalAgent,
                TaskKind::Workflow => wire::TaskKind::Workflow,
                TaskKind::Other => wire::TaskKind::Other,
            } as i32,
            task_type: task.task_type.clone(),
            description: task.description.clone(),
            subagent_type: task.subagent_type.clone(),
            parent_task_id: task.parent_task_id.clone(),
            tool_use_id: task.tool_use_id.clone(),
            ambient: task.ambient,
            workflow_name: task.workflow_name.clone(),
        }
    }

    fn through_wire(event: &AgentDomainEvent) -> AgentDomainEvent {
        match event {
            AgentDomainEvent::BackgroundTasksChanged { tasks, occurred_at } => translate_322::roster(
                wire::BackgroundTasksChanged {
                    tasks: tasks.iter().map(wire_task).collect(),
                },
                *occurred_at,
            ),
            AgentDomainEvent::TaskEvent {
                edge,
                task_id,
                run_id,
                task,
                status,
                summary,
                last_tool_name,
                usage,
                end_time_ms,
                error,
                is_backgrounded,
                reason,
                output_file,
                workflow_progress,
                occurred_at,
            } => {
                // Stopping is the host's pending-stop state, which has no wire enum.
                if *status == Some(TaskStatus::Stopping) {
                    return event.clone();
                }
                translate_322::task(
                    wire::TaskEvent {
                        edge: match edge {
                            TaskEdge::Started => wire::TaskEdge::Started,
                            TaskEdge::Progress => wire::TaskEdge::Progress,
                            TaskEdge::Updated => wire::TaskEdge::Updated,
                            TaskEdge::Notified => wire::TaskEdge::Notified,
                        } as i32,
                        task_id: task_id.clone(),
                        run_id: run_id.clone(),
                        task: task.as_ref().map(wire_task),
                        status: status.map(|s| match s {
                            TaskStatus::Running => wire::TaskStatus::Running,
                            TaskStatus::Paused => wire::TaskStatus::Paused,
                            TaskStatus::Completed => wire::TaskStatus::Completed,
                            TaskStatus::Failed => wire::TaskStatus::Failed,
                            TaskStatus::Stopped => wire::TaskStatus::Stopped,
                            TaskStatus::Stopping | TaskStatus::Ended => unreachable!(),
                        } as i32),
                        summary: summary.clone(),
                        last_tool_name: last_tool_name.clone(),
                        usage: usage.as_ref().map(|u| wire::TaskUsage {
                            total_tokens: u.total_tokens,
                            tool_uses: u.tool_uses,
                            duration_ms: u.duration_ms,
                        }),
                        end_time_ms: *end_time_ms,
                        error: error.clone(),
                        is_backgrounded: *is_backgrounded,
                        reason: reason.clone(),
                        output_file: output_file.clone(),
                        workflow_progress: workflow_progress.as_ref().map(|p| wire::WorkflowProgress {
                            phases: p
                                .phases
                                .iter()
                                .map(|p| wire::WorkflowPhase {
                                    index: p.index,
                                    title: p.title.clone(),
                                    kind: p.kind.clone(),
                                })
                                .collect(),
                            agents: p
                                .agents
                                .iter()
                                .map(|a| wire::WorkflowAgent {
                                    index: a.index,
                                    label: a.label.clone(),
                                    phase_index: a.phase_index,
                                    state: a.state.clone(),
                                    model: a.model.clone(),
                                    agent_type: a.agent_type.clone(),
                                    tokens: a.tokens,
                                    tool_calls: a.tool_calls,
                                    duration_ms: a.duration_ms,
                                    error: a.error.clone(),
                                })
                                .collect(),
                        }),
                    },
                    *occurred_at,
                )
                .unwrap()
            }
            _ => event.clone(),
        }
    }

    #[test]
    fn every_background_case_count_equals_the_roster() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/background-tasks.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            let mut state = AgentSessionProjection::default();
            let mut latest = 0;
            for (index, event) in events.iter().enumerate() {
                let old = state.background_tasks.running();
                let derived = state.apply_with_derived(&through_wire(event));
                let running = state.background_tasks.running();
                assert_eq!(derived.len(), usize::from(old != running), "{name} event {index}");
                for event in derived {
                    let AgentDomainEvent::BackgroundCalls { running } = event else {
                        panic!("unexpected derivation: {name}")
                    };
                    latest = running;
                }
                assert_eq!(latest, running, "{name} event {index}");
                assert_eq!(state.background_calls, running, "{name} event {index}");
                assert_eq!(
                    running as u64,
                    case["expected_running"][index].as_u64().unwrap(),
                    "{name} event {index}"
                );
                assert!(state.background_tasks.rows.len() <= 276, "{name}");
                if let Some(expected) = case["expected_prefix_rows"].get(index.to_string()) {
                    let rows = serde_json::to_value(&state.background_tasks.rows).unwrap();
                    for (actual, expected) in rows.as_array().unwrap().iter().zip(expected.as_array().unwrap()) {
                        for (key, value) in expected.as_object().unwrap() {
                            assert_eq!(&actual[key], value, "{name} event {index} {key}");
                        }
                    }
                }
            }
            assert_eq!(
                state.notices.len() as u64,
                case["expected_notice_count"].as_u64().unwrap(),
                "{name}"
            );
            if let Some(expected) = case.get("expected_notice") {
                let notice = serde_json::to_value(&state.notices[0]).unwrap();
                for (key, value) in expected.as_object().unwrap() {
                    assert_eq!(&notice[key], value, "{name} {key}");
                }
            }
            let rows = serde_json::to_value(&state.background_tasks.rows).unwrap();
            assert_eq!(
                rows.as_array().unwrap().len(),
                case["expected_rows"].as_array().unwrap().len(),
                "{name}"
            );
            for (actual, expected) in rows
                .as_array()
                .unwrap()
                .iter()
                .zip(case["expected_rows"].as_array().unwrap())
            {
                for (key, value) in expected.as_object().unwrap() {
                    assert_eq!(&actual[key], value, "{name} {key}");
                }
            }
        }
    }
}
