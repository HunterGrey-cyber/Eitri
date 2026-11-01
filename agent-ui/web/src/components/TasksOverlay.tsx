import { forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState } from "react";
import { elapsedWords, liveTasks } from "../band";
import { nextRequestId } from "../bridge";
import { toolCallPath } from "../externalTools";
import { TASKS_KEY_LINE, tasksKey, type KeyLike } from "../keymap";
import { revealedText } from "../revealHidden";
import { formatResultContent } from "../toolRegistry";
import type { AgentUiState, BackgroundTaskRecord, TaskKind, ToolCallRecord } from "../types";

type Reply = { requestId: string; taskId?: string; output?: string; totalBytes?: number; truncated?: boolean; error?: string };
export type TasksOverlayHandle = { key: (event: KeyLike) => void; reply: (reply: Reply) => void; wake: (count: number) => void };
type Props = {
  state: AgentUiState;
  workflows?: boolean;
  initialTaskId?: string | null;
  onClose: () => void;
  onRequest: (kind: "stop_task" | "task_output", taskId: string, requestId: string) => void;
  onOpen: (title: string, text: string) => void;
};

const GROUPS: [TaskKind | "completed", string][] = [
  ["shell", "Shells"], ["monitor", "Monitors"], ["local_agent", "Local agents"],
  ["workflow", "Dynamic workflows"], ["other", "Other tasks"], ["completed", "Completed"],
];
const finished = (task: BackgroundTaskRecord) => ["completed", "failed", "stopped", "ended"].includes(task.status);
// A row ended by an event with no clock time carries its sequence number as `ended_at` (and as `finished_seq`).
const endedBySeq = (task: BackgroundTaskRecord) => task.ended_at !== null && task.ended_at === task.finished_seq;
const identity = (task: BackgroundTaskRecord) => JSON.stringify([task.task_id, task.run_id, task.started_at]);
const glyph = (task: BackgroundTaskRecord) => task.status === "paused" ? "⏸" : task.status === "completed" ? "✓"
  : task.status === "failed" ? "✕" : ["stopped", "ended", "stopping"].includes(task.status) ? "■" : "●";

const LOADING = "loading output…";
/** How often a running shell's or monitor's tail is read again while its detail is open. */
export const OUTPUT_POLL_MS = 1000;

/** What the detail says about the tail it shows: how many lines, and of how many bytes when the host cut it. */
export function shownWords(text: string, total: number, truncated: boolean): string {
  const lines = text.replace(/\n$/, "").split("\n").length;
  const count = `${lines} ${lines === 1 ? "line" : "lines"}`;
  return truncated ? `Showing the last ${count} of ${total} bytes` : `Showing ${count}`;
}

/** Terminal escape payloads never become output text, including OSC links and their targets. */
export function taskOutputText(text: string): string {
  return revealedText(text
    .replace(/(?:\x1b\]|\x9d)[\s\S]*?(?:\x07|\x1b\\|\x9c|$)/g, "")
    .replace(/(?:\x1b[P^_X]|[\x90\x98\x9e\x9f])[\s\S]*?(?:\x1b\\|\x9c|$)/g, "")
    .replace(/(?:\x1b\[|\x9b)[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[ -/]*[@-~]/g, ""));
}

export function stopTaskWords(rows: BackgroundTaskRecord[]): string {
  const tasks = liveTasks(rows);
  return ([ ["shell", "shell"], ["monitor", "monitor"], ["local_agent", "local agent"],
    ["workflow", "background dynamic workflow"], ["other", "background task"] ] as const)
    .flatMap(([kind, word]) => {
      const n = tasks.filter((task) => task.kind === kind).length;
      return n ? [`${n} ${word}${n === 1 ? "" : "s"}`] : [];
    }).join(", ");
}

/** A workflow phase's line in the detail: its title, its kind when the CLI gave one, and its agents. */
export function phaseLine(phase: { title: string; kind: string }, agents: number): string {
  return [phase.title, phase.kind, `${agents} ${agents === 1 ? "agent" : "agents"}`].filter((field) => field !== "").join(" · ");
}

function usage(task: BackgroundTaskRecord): string {
  return task.usage ? `${task.usage.total_tokens} tokens · ${task.usage.tool_uses} tool calls` : "";
}

function taskFacts(task: BackgroundTaskRecord, state: AgentUiState): string {
  const facts = [task.status, task.subagent_type, usage(task)];
  if (task.kind === "monitor") facts.push(`${state.notices.filter((n) => n.taskId === task.task_id && n.taskStatus === "running").length} events`);
  if (task.kind === "workflow" && state.capabilities.workflowProgress && task.workflow_progress) {
    const agents = task.workflow_progress.agents;
    facts.push(`${agents.filter((a) => ["done", "completed", "failed", "stopped"].includes(a.state)).length}/${agents.length} agents`);
  }
  if (task.kind === "local_agent") {
    const input = state.toolCalls.find((call) => call.toolUseId === task.tool_use_id)?.input;
    if (input && typeof input === "object") {
      for (const key of ["model", "effort"]) {
        const value = (input as Record<string, unknown>)[key];
        if (typeof value === "string") facts.push(value);
      }
    }
  }
  if (task.status === "failed") facts.push(task.error ?? task.summary);
  return revealedText(facts.filter(Boolean).join(" · "));
}

export const TasksOverlay = forwardRef<TasksOverlayHandle, Props>(function TasksOverlay(
  { state, workflows = false, initialTaskId = null, onClose, onRequest, onOpen }, ref,
) {
  const root = useRef<HTMLDivElement>(null);
  const [now, setNow] = useState(Date.now);
  // When this overlay first saw a row that ended without a clock time: its thirty seconds count from there.
  const firstSeenEnded = useRef(new Map<string, number>());
  const endedAt = (t: BackgroundTaskRecord): number | null => {
    if (t.ended_at === null || !endedBySeq(t)) return t.ended_at;
    const seen = firstSeenEnded.current.get(identity(t)) ?? now;
    firstSeenEnded.current.set(identity(t), seen);
    return seen;
  };
  const shown = (t: BackgroundTaskRecord) => {
    const end = endedAt(t);
    return !finished(t) || (end !== null && now - end < 30_000);
  };
  const elapsed = (t: BackgroundTaskRecord) => endedBySeq(t) ? "" : elapsedWords((t.ended_at ?? now) - t.started_at);
  const [selected, setSelected] = useState<string | null>(() => initialTaskId ??
    (workflows ? state.backgroundTasks.rows.find((t) => t.kind === "workflow" && !t.ambient && !finished(t) && shown(t))?.task_id : null) ?? null);
  const [detail, setDetail] = useState(initialTaskId !== null);
  const [systemOpen, setSystemOpen] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [updates, setUpdates] = useState(0);
  const [output, setOutput] = useState<{ identity: string; text: string; total: number; truncated: boolean } | null>(null);
  const pending = useRef(new Map<string, { task: BackgroundTaskRecord; open: boolean; kind: "stop_task" | "task_output"; again: boolean }>());
  // Where a refresh found the scroll position, so that the new tail leaves a reader who scrolled up in place.
  const keepScroll = useRef<{ top: number; atBottom: boolean } | null>(null);
  // Whether the line now up came from an output read: a later good read clears that line and no other.
  const outputOwnsMessage = useRef(false);
  const say = (text: string | null, fromOutput = false) => { outputOwnsMessage.current = fromOutput && text !== null; setMessage(text); };
  // The run whose detail has been seen running: when it ends, its output is read once more.
  const seenRunning = useRef<string | null>(null);
  const visible = state.capabilities.backgroundTasks ? state.backgroundTasks.rows.filter(shown) : [];
  const groups = GROUPS.map(([kind, title]) => ({ kind, title, rows: visible.filter((t) => !t.ambient &&
    (kind === "completed" ? finished(t) : !finished(t) && t.kind === kind)) }));
  const ambient = visible.filter((t) => t.ambient);
  const rows = [...groups.flatMap((g) => g.rows), ...(systemOpen ? ambient : [])];
  const systemSelected = selected === "__system__";
  const task = systemSelected ? undefined : rows.find((t) => t.task_id === selected) ?? rows[0];
  const shownOutput = task && output?.identity === identity(task) ? output : null;

  useEffect(() => {
    root.current?.focus();
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  useEffect(() => {
    if (initialTaskId !== null && task) view(task);
    if (workflows) root.current?.querySelector<HTMLElement>('[data-task-group="workflow"]')?.scrollIntoView?.({ block: "start" });
  }, []);
  useEffect(() => {
    root.current?.querySelector<HTMLElement>("[aria-selected=true]")?.scrollIntoView?.({ block: "nearest" });
  }, [task?.task_id, detail]);
  useEffect(() => {
    if (detail && selected !== null && !rows.some((row) => row.task_id === selected)) setDetail(false);
  }, [detail, selected, rows]);

  const latest = useRef({ task, request: (_kind: "stop_task" | "task_output", _task: BackgroundTaskRecord, _open?: boolean, _silent?: boolean, _last?: boolean) => {} });
  latest.current = { task, request };
  const reading = detail && task !== undefined && (task.kind === "shell" || task.kind === "monitor") && state.capabilities.taskOutput;
  const readingId = reading ? identity(task) : null;
  const ended = task !== undefined && finished(task);
  useEffect(() => {
    if (readingId === null) return;
    const read = (last = false) => { if (latest.current.task) latest.current.request("task_output", latest.current.task, false, true, last); };
    if (!ended) {
      seenRunning.current = readingId;
      const timer = window.setInterval(() => read(), OUTPUT_POLL_MS);
      return () => window.clearInterval(timer);
    }
    if (seenRunning.current === readingId) { seenRunning.current = null; read(true); }
  }, [readingId, ended]);
  useLayoutEffect(() => {
    const keep = keepScroll.current, el = root.current;
    keepScroll.current = null;
    if (keep && el) el.scrollTop = keep.atBottom ? el.scrollHeight : keep.top;
  }, [output]);

  /** `last` is the read made when the task ended: one still in flight began before that, so it is followed by another. */
  function request(kind: "stop_task" | "task_output", current: BackgroundTaskRecord, open = false, silent = false, last = false) {
    if (kind === "stop_task" && !state.capabilities.taskControl) { say("this sidecar cannot stop one task"); return; }
    if (kind === "stop_task" && (!current.live || finished(current))) { say("this task has ended"); return; }
    if (kind === "task_output" && !state.capabilities.taskOutput) { say("output is not available from this sidecar"); return; }
    if (kind === "task_output" && current.kind !== "shell" && current.kind !== "monitor") {
      if (open) say("this task has no output file");
      return;
    }
    if (pending.current.size >= 64) { if (!silent) say("too many task commands are waiting"); return; }
    const existing = [...pending.current.values()].find((p) => p.kind === kind && identity(p.task) === identity(current));
    if (existing) {
      existing.open ||= open;
      existing.again ||= last;
      if (!silent && kind === "task_output") say(LOADING, true);
      return;
    }
    const requestId = nextRequestId();
    pending.current.set(requestId, { task: current, open, kind, again: false });
    if (!silent) say(kind === "task_output" ? LOADING : "stopping…", kind === "task_output");
    onRequest(kind, current.task_id, requestId);
  }
  function view(current: BackgroundTaskRecord) {
    setSelected(current.task_id); setDetail(true); say(null); setOutput(null);
    if (current.kind === "shell" || current.kind === "monitor") request("task_output", current);
  }
  /** The overlay is the scroller: a detail view taller than it is read with these keys. */
  function scroll(action: "next" | "previous" | "pageDown" | "pageUp" | "top" | "bottom") {
    const el = root.current;
    if (!el) return;
    const page = Math.max(40, Math.floor((el.clientHeight || 200) * 0.9));
    if (action === "top") el.scrollTop = 0;
    else if (action === "bottom") el.scrollTop = el.scrollHeight;
    else el.scrollTop = Math.max(0, el.scrollTop + { next: 40, previous: -40, pageDown: page, pageUp: -page }[action]);
  }
  function key(event: KeyLike) {
    const action = tasksKey(event);
    if (action === "pageDown" || action === "pageUp" || action === "top" || action === "bottom" ||
      ((action === "next" || action === "previous") && detail)) {
      scroll(action);
    } else if (action === "back") {
      if (detail) { setDetail(false); say(null); } else onClose();
    } else if ((action === "next" || action === "previous") && !detail) {
      const ids = [...groups.flatMap((g) => g.rows.map((t) => t.task_id)), "__system__", ...(systemOpen ? ambient.map((t) => t.task_id) : [])];
      const index = systemSelected ? ids.indexOf("__system__") : task ? ids.indexOf(task.task_id) : 0;
      setSelected(ids[Math.max(0, Math.min(ids.length - 1, index + (action === "next" ? 1 : -1)))]);
      say(null);
    } else if (systemSelected && action === "view") {
      setSystemOpen((v) => !v);
    } else if (task) {
      if (action === "view" && !detail) view(task);
      if (action === "stop") request("stop_task", task);
      if (action === "open") request("task_output", task, true);
    }
  }
  useImperativeHandle(ref, () => ({ key, wake: (count) => setUpdates((n) => n + count), reply(reply) {
    const waiting = pending.current.get(reply.requestId);
    if (!waiting) return;
    pending.current.delete(reply.requestId);
    // A delayed tail belongs to the run that requested it, never a reused id or a different selection.
    if (!task || identity(task) !== identity(waiting.task) || (reply.taskId !== undefined && reply.taskId !== task.task_id)) return;
    // A read begun before the task ended may have missed its last lines: read again.
    if (waiting.again) latest.current.request("task_output", task, false, true);
    if (reply.error !== undefined) { say(revealedText(reply.error), waiting.kind === "task_output"); return; }
    if (waiting.kind === "stop_task") { say("stop requested"); return; }
    if (reply.output === undefined) return;
    const text = taskOutputText(reply.output);
    const next = { identity: identity(task), text, total: reply.totalBytes ?? 0, truncated: reply.truncated ?? false };
    const el = root.current;
    if (shownOutput && el && (shownOutput.text !== text || shownOutput.total !== next.total || shownOutput.truncated !== next.truncated)) {
      keepScroll.current = { top: el.scrollTop, atBottom: el.scrollHeight - el.scrollTop - el.clientHeight <= 4 };
    }
    setOutput((prev) => prev && prev.identity === next.identity && prev.text === text && prev.total === next.total &&
      prev.truncated === next.truncated ? prev : next);
    // Only a line an output read put up goes: a refresh must not wipe a stop request's answer.
    if (outputOwnsMessage.current) say(null);
    if (waiting.open) onOpen("Task output", text);
  } }));

  const path = task && toolCallPath(state.toolCalls, task.tool_use_id);
  const launchCall = path ? path[path.length - 1] : null;
  const launch = launchCall?.result;
  const launchInput = launchCall?.input;
  const command = task && (task.kind === "shell" || task.kind === "monitor") && launchInput && typeof launchInput === "object" &&
    typeof (launchInput as Record<string, unknown>).command === "string" ? (launchInput as { command: string }).command : null;
  return (
    <div className="tasks-overlay" role="dialog" aria-modal="true" aria-label="Background tasks" tabIndex={-1} ref={root}
      onKeyDown={(event) => { event.preventDefault(); event.stopPropagation(); key(event.nativeEvent); }}
      onClick={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <header className="review-header"><span className="review-title">Background tasks</span>
        <button className="tasks-close" type="button" onClick={onClose}>Close</button></header>
      {!state.capabilities.backgroundTasks ? <p>this Claude Code sidecar does not report background work</p> : detail && task ? (
        <section className="tasks-detail">
          <h3>{glyph(task)} {revealedText(task.description)}</h3>
          <div>{[taskFacts(task, state), elapsed(task)].filter(Boolean).join(" · ")}</div>
          {(task.kind === "shell" || task.kind === "monitor") && <>
            {command !== null && <><div className="tasks-dim">Command</div><pre className="tasks-command">{revealedText(command)}</pre></>}
            {shownOutput && (shownOutput.text === "" ? <p className="tasks-dim">No output available</p> : <>
              <pre>{shownOutput.text}</pre>
              <div className="tasks-dim">{shownWords(shownOutput.text, shownOutput.total, shownOutput.truncated)}</div>
            </>)}
            {task.output_file && <div className="tasks-dim">{revealedText(task.output_file)}</div>}
          </>}
          {task.kind === "local_agent" && <>
            {task.summary && <p>{revealedText(task.summary)}</p>}
            {task.last_tool_name && <p>Last tool: {revealedText(task.last_tool_name)}</p>}
            {launchCall?.children && <AgentSteps calls={launchCall.children.toolCalls} />}
          </>}
          {task.kind === "workflow" && <>
            {state.capabilities.workflowProgress && task.workflow_progress && <>
              {task.workflow_progress.phases.map((phase) => <div key={phase.index}>
                {revealedText(phaseLine(phase, task.workflow_progress!.agents.filter((a) => a.phase_index === phase.index).length))}
              </div>)}
              {task.workflow_progress.agents.map((agent) => <div className="tasks-agent" key={agent.index}>
                {revealedText([agent.label, agent.state, agent.model, agent.agent_type,
                  agent.tokens === null ? null : `${agent.tokens} tokens`, agent.tool_calls === null ? null : `${agent.tool_calls} tool calls`, agent.error].filter(Boolean).join(" · "))}
              </div>)}
            </>}
            {state.capabilities.workflowProgress && launch && <pre>{revealedText(formatResultContent(launch.content))}</pre>}
          </>}
          {task.error && (task.kind !== "workflow" || state.capabilities.workflowProgress) && <p>{revealedText(task.error)}</p>}
          {task.summary?.includes("stopped after reaching its background time limit") && <p className="tasks-dim">
            Claude Code stops a background command started without a timeout after 30 minutes in an app; Claude was told and can start it again with a longer timeout.
          </p>}
        </section>
      ) : <div role="listbox" aria-label="Tasks">
        {groups.filter((g) => g.kind !== "other" || g.rows.length > 0).map((group) => <section key={group.kind} data-task-group={group.kind}>
          <h3>{group.title} ({group.rows.length})</h3>
          {group.rows.map((row) => <TaskRow key={identity(row)} task={row} selected={row === task} state={state} elapsed={elapsed(row)} onView={() => view(row)} />)}
        </section>)}
        <button className="tasks-system" type="button" aria-expanded={systemOpen} data-selected={systemSelected} onClick={() => { setSelected("__system__"); setSystemOpen((v) => !v); }}>
          {systemOpen ? "▾" : "▸"} System tasks ({ambient.length})
        </button>
        {systemOpen && ambient.map((row) => <TaskRow key={identity(row)} task={row} selected={row === task} state={state} elapsed={elapsed(row)} onView={() => view(row)} />)}
      </div>}
      {message && <p role="status">{message}</p>}
      {updates > 0 && <div className="tasks-dim">{updates} {updates === 1 ? "update" : "updates"} below</div>}
      <footer className="review-keys">{TASKS_KEY_LINE}</footer>
    </div>
  );
});

function AgentSteps({ calls }: { calls: ToolCallRecord[] }) {
  if (calls.length === 0) return null;
  return <ol aria-label="Agent steps">
    {calls.map((call) => <li key={call.seq}>
      <div>{revealedText(call.name)} · {call.result ? call.result.isError ? "failed" : "completed" : "pending"}</div>
      <pre>{revealedText(JSON.stringify(call.input, null, 2) ?? "")}</pre>
      {call.result && <pre>{revealedText(formatResultContent(call.result.content))}</pre>}
      {call.children && <AgentSteps calls={call.children.toolCalls} />}
    </li>)}
  </ol>;
}

function TaskRow({ task, selected, state, elapsed, onView }: {
  task: BackgroundTaskRecord; selected: boolean; state: AgentUiState; elapsed: string; onView: () => void;
}) {
  return <div role="option" aria-selected={selected} className="tasks-row" onClick={onView}>
    <span aria-hidden="true">{glyph(task)}</span>
    <div><div>{revealedText(task.description)}</div><div className="tasks-dim">{taskFacts(task, state)}</div></div>
    <span className="tasks-elapsed">{elapsed}</span>
  </div>;
}
