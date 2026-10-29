import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, BackgroundTask, BackgroundTaskRecord, NoticeRecord, TaskRoster, TaskStatus } from "../types";

function compareRun(left: string, right: string): number {
  const a = Array.from(left, (c) => c.codePointAt(0)!);
  const b = Array.from(right, (c) => c.codePointAt(0)!);
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

const terminal = (status: TaskStatus) => ["completed", "failed", "stopped", "ended"].includes(status);
export const emptyRoster = (): TaskRoster => ({ rows: [], query_start: false });

function fresh(task: BackgroundTask, seq: number): BackgroundTaskRecord {
  return { ...task, status: "running", live: false, started_at: seq, ended_at: null, finished_seq: null,
    summary: null, last_tool_name: null, usage: null, error: null, is_backgrounded: null,
    reason: null, output_file: null, workflow_progress: null };
}
function end(row: BackgroundTaskRecord, status: TaskStatus, seq: number, at: number) {
  // A second terminal edge restates the outcome; the task ended at the first recorded moment.
  if (!terminal(row.status) || row.ended_at === null) row.ended_at = at;
  row.status = status; row.live = false; row.finished_seq = seq;
}
function rowFor(roster: TaskRoster, taskId: string, run: string | null, task: BackgroundTask | null, seq: number, provisional: boolean): BackgroundTaskRecord | null {
  let row = roster.rows.find((r) => r.task_id === taskId);
  if (row) {
    if (run !== null && row.run_id !== null) {
      if (compareRun(run, row.run_id) < 0) return null;
      if (compareRun(run, row.run_id) > 0) Object.assign(row, fresh({ ...(task ?? row), run_id: run }, seq));
    }
    if (task) {
      const joined = provisional && row.kind !== "shell" && task.kind === "shell";
      const kind = row.kind, tool = row.tool_use_id;
      const previous = { ...row };
      Object.assign(row, task);
      row.run_id ??= previous.run_id;
      row.tool_use_id ??= previous.tool_use_id;
      row.workflow_name ??= previous.workflow_name;
      row.subagent_type ??= previous.subagent_type;
      row.parent_task_id ??= previous.parent_task_id;
      if (!row.description) row.description = previous.description;
      if (!row.task_type) row.task_type = previous.task_type;
      if (joined) { row.kind = kind; row.tool_use_id = tool ?? row.tool_use_id; }
    }
    row.task_id = taskId;
    if (run !== null) row.run_id = run;
    return row;
  }
  row = fresh(task ?? { task_id: taskId, run_id: run, kind: "other", task_type: "", description: "",
    subagent_type: null, parent_task_id: null, tool_use_id: null, ambient: false, workflow_name: null }, seq);
  row.task_id = taskId;
  if (run !== null) row.run_id = run;
  roster.rows.push(row);
  return row;
}
function update(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  const roster: TaskRoster = { ...state.backgroundTasks, rows: state.backgroundTasks.rows.map((r) => ({ ...r })) };
  const seq = state.nextSeq - 1;
  const at = (event.type === "background_tasks_changed" || event.type === "task_event") ? event.occurred_at ?? seq : seq;
  let notice: NoticeRecord | null = null;
  switch (event.type) {
    case "background_tasks_changed": {
      if (event.tasks.length === 0 && roster.query_start) roster.rows = [];
      roster.query_start = false;
      const previousLive = new Map(roster.rows.map((r) => [r.task_id, r.live]));
      for (const row of roster.rows) row.live = false;
      for (const task of event.tasks.slice(0, 256)) {
        const row = rowFor(roster, task.task_id, task.run_id, task, at, true);
        if (row) {
          row.live = true;
          if (row.status !== "stopping" && row.status !== "paused") row.status = "running";
          row.ended_at = null;
          row.finished_seq = null;
        } else {
          const current = roster.rows.find((r) => r.task_id === task.task_id);
          if (current) current.live = previousLive.get(task.task_id) ?? false;
        }
      }
      for (const row of roster.rows) if (!row.live && row.status === "stopping") end(row, "stopped", seq, at);
      break;
    }
    case "task_event": {
      if (event.task === null && event.status === null && event.summary === null && event.last_tool_name === null
        && event.usage === null && event.error === null && event.is_backgrounded === null && event.reason === null
        && event.output_file === null && event.workflow_progress === null && event.end_time_ms === null
        && !roster.rows.some((r) => r.task_id === event.task_id)) return state;
      const row = rowFor(roster, event.task_id, event.run_id ?? event.task?.run_id ?? null, event.task, at, false);
      if (!row) return state;
      if (event.summary !== null) row.summary = event.summary;
      if (event.last_tool_name !== null) row.last_tool_name = event.last_tool_name;
      if (event.usage !== null) row.usage = event.usage;
      if (event.error !== null) row.error = event.error;
      if (event.is_backgrounded !== null) row.is_backgrounded = event.is_backgrounded;
      if (event.reason !== null) row.reason = event.reason;
      if (event.output_file !== null) row.output_file = event.output_file;
      if (event.edge === "progress") row.workflow_progress = event.workflow_progress;
      if (event.edge === "started" && event.occurred_at !== undefined) row.started_at = at;
      if (event.edge === "started" && event.is_backgrounded === true && !terminal(row.status)) row.live = true;
      if (event.status !== null) {
        if (terminal(event.status)) end(row, event.status, seq, at);
        else if (!terminal(row.status) && row.status !== "stopping") row.status = event.status;
      }
      if (event.reason === "worker_restart") { end(row, "stopped", seq, at); row.summary = "stopped (Claude Code restarted)"; }
      // The CLI's end time beats arrival time, for a row that has ended only.
      if (event.end_time_ms !== null && terminal(row.status)) row.ended_at = event.end_time_ms;
      if (event.edge === "notified" && event.status !== null && (terminal(event.status) || event.status === "running") && !row.ambient) {
        notice = { seq, kind: "task", level: "notice", content: event.summary, scalars: {},
          taskId: event.task_id, toolUseId: row.tool_use_id, taskStatus: event.status };
      }
      break;
    }
    case "turn_completed":
      for (const row of roster.rows) if (!row.live && !terminal(row.status)) end(row, "ended", seq, at);
      break;
    // Not session_opened: the CLI re-sends its init on every turn of one process.
    case "session_closed": case "session_unavailable": case "resume_outcome":
      for (const row of roster.rows) if (!terminal(row.status)) { end(row, "ended", seq, at); row.summary = "stopped with the session"; }
      // A gone process makes the next level the new query's empty one; a resume's outcome comes after it.
      if (event.type !== "resume_outcome") roster.query_start = roster.rows.length > 0;
      break;
    default: return state;
  }
  roster.rows.sort((a, b) => a.finished_seq === b.finished_seq ? Number(b.live) - Number(a.live) : a.finished_seq === null ? 1 : b.finished_seq === null ? -1 : b.finished_seq - a.finished_seq);
  let pending = 0, finished = 0;
  roster.rows = roster.rows.filter((row) => terminal(row.status) ? ++finished <= 20 : ++pending <= 256);
  return { ...state, backgroundTasks: roster, ...(notice ? { notices: [...state.notices, notice], assistantMessageOpen: false, thinkingOpen: false } : {}) };
}

export function fold(state: AgentUiState, event: AgentDomainEvent): AgentUiState { return update(state, event); }

export function fromSnapshot(state: AgentUiState, snapshot: AgentUiSnapshot): AgentUiState {
  return { ...state, backgroundTasks: snapshot.backgroundTasks ?? emptyRoster() };
}

export function onTurnCompleted(state: AgentUiState, event: AgentDomainEvent): AgentUiState { return update(state, event); }
export function onSessionEnd(state: AgentUiState, event: AgentDomainEvent): AgentUiState { return update(state, event); }
