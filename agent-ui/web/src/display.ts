import type { TimelineItem } from "./timeline";
import type { AgentUiState, ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "./types";
import { shortToolName, subagentOf } from "./externalTools";

/** `thinking` is the panel's `agent.thinking` level; absent reads as `collapsed`, its default. */
type Options = { expanded: Record<string, boolean>; detailed: boolean; turnRunning: boolean; thinking?: ThinkingDisplay };

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** What a run of `n` calls of one tool did, in words, for the tools that have a plain verb; any
 *  other tool keeps its name and a count. */
const RUN_PHRASES: Record<string, (n: number) => string> = {
  Read: (n) => `read ${plural(n, "file", "files")}`,
  Grep: (n) => `searched ${plural(n, "pattern", "patterns")}`,
  Glob: (n) => `listed ${plural(n, "pattern", "patterns")}`,
  WebSearch: (n) => plural(n, "web search", "web searches"),
  WebFetch: (n) => `fetched ${plural(n, "page", "pages")}`,
  Bash: (n) => `ran ${plural(n, "command", "commands")}`,
};

/** `Read 3 files, ran 2 commands`, tools in first-seen order (Codex's `Explored` cell and Claude
 *  Code's `Read 3 files`). */
export function runSummary(calls: ToolCallRecord[]): string {
  const counts = new Map<string, number>();
  for (const c of calls) counts.set(c.name, (counts.get(c.name) ?? 0) + 1);
  const text = Array.from(counts, ([name, n]) =>
    Object.prototype.hasOwnProperty.call(RUN_PHRASES, name) ? RUN_PHRASES[name](n) : `${shortToolName(name)} ×${n}`,
  ).join(", ");
  return calls.length > 0 && Object.prototype.hasOwnProperty.call(RUN_PHRASES, calls[0].name)
    ? text.charAt(0).toUpperCase() + text.slice(1)
    : text;
}

/** With the acceptEdits fast path, a `Write`/`Edit`/`NotebookEdit` can now
 *  finish with no card at all, exactly as an ordinary read does -- so without this exclusion the
 *  loop below would fold it into a `Read 4 files, Edit ×1`-shaped run and the change it made would
 *  never be its own row. A run of other tools now stops at one of these three, on both sides, the
 *  same way a failed or still-gated call already does; reads and searches keep folding as before. */
const NEVER_FOLDED_TOOLS = new Set([
  "Write", "Edit", "NotebookEdit", "fileChange", "Monitor", "Agent", "Task", "TaskStop", "TaskOutput",
  "Workflow", "RunWorkflow", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList", "SendMessage", "ListAgents",
]);

/** The first words of a `Bash` result that launched a background task (started so, or moved there after its
 *  timeout). Anchored at the start so a foreground command that merely prints the phrase is not taken for one. */
const BASH_BACKGROUND_LAUNCH =
  /^Command (?:running in background with ID:|(?:did not complete within [^\n]{1,60}? and )?was moved to the background \(ID:)/;

/** The same for an ACP agent's file-changing kinds: the change is its own row, not a count in a run. */
const neverFolded = (call: ToolCallRecord): boolean =>
  NEVER_FOLDED_TOOLS.has(call.name) ||
  call.background === true ||
  (call.name === "Bash" &&
    (field(call.input, "run_in_background") === true || BASH_BACKGROUND_LAUNCH.test(resultText(call)))) ||
  /^acp:(edit|delete|move)$/.test(call.name) ||
  // A subagent's call keeps its own row, so whose it is stays on screen instead of inside a count.
  subagentOf(call.name, call.input) !== null;

/** Consecutive finished, ungated tool calls -- two or more, with something after
 *  them or a turn that is over -- become one `run` row. Expanded runs and the detailed view keep
 *  every call.
 *
 * A failed call never joins a run: every run row draws a fixed `✓` sign and
 * `runSummary` names only counts, so a failing call folded in among successes vanished behind a
 * success sign until the reader expanded the row. Excluding it from the loop below ends the run at
 * the failure -- the failing call falls out to the single-item path just below, and its own row
 * draws `✗` exactly as an unfolded failed call always has (`MessageList.tsx`'s `toolSign`).
 *
 * Thinking (`opts.thinking`): under `hidden` its rows are left out before anything is folded, so runs
 * fold exactly as they would with no thinking at all. Under `collapsed` and `preview` a finished
 * thinking block that sits BETWEEN two calls of a run is folded into the run with them: an agent that
 * thinks before every call would otherwise break every run into single rows, one thought row between
 * each, and a run exists to keep that quiet. Expanding the run puts the thoughts back as their own rows.
 * A thought before the first call or after the last stays a row of its own, as does one still being
 * written. Under `expanded` no thought is folded away. The detailed view keeps everything, thinking
 * included, whatever the level.
 *
 * A run never crosses from one turn into the next (a call or thought of another turn ends it): a turn's
 * "Worked for" line stands under the turn's last row, and a run holding two turns' calls would leave it
 * nowhere to stand. An item that names no turn joins either side. */
export function buildDisplay(timeline: TimelineItem[], opts: Options): TimelineItem[] {
  const tree: TimelineItem[] = [];
  const visit = (item: TimelineItem, depth: number) => {
    tree.push(depth === 0 ? item : { ...item, depth });
    if (item.kind !== "tool" || !isAgentCall(item.call) || isBackgroundAgent(item.call) ||
        !(opts.expanded[item.key] || opts.detailed) || !item.call.children) return;
    const stream = item.call.children;
    const children: TimelineItem[] = [
      ...stream.toolCalls.map((call): TimelineItem => ({ kind: "tool", seq: call.seq, key: `t-${call.seq}`, call })),
      ...stream.transcript.map((message): TimelineItem => ({ kind: "message", seq: message.seq, key: `m-${message.seq}`, text: message.text })),
      ...stream.thinking.map((block, i): TimelineItem => ({ kind: "thinking", seq: block.seq, key: `k-${block.seq}`, block,
        open: stream.thinkingOpen && i === stream.thinking.length - 1 })),
    ];
    children.sort((a, b) => a.seq - b.seq).forEach((child) => visit(child, depth + 1));
  };
  timeline.forEach((item) => visit(item, 0));
  timeline = tree;
  if (opts.detailed) return timeline;
  const level = opts.thinking ?? "collapsed";
  const items = level === "hidden" ? timeline.filter((item) => item.kind !== "thinking") : timeline;
  const absorbThoughts = level === "collapsed" || level === "preview";
  const gated = new Set(
    items.flatMap((item) => (item.kind === "permission" && item.request.toolUseId ? [item.request.toolUseId] : [])),
  );
  const foldable = (item: TimelineItem): item is Extract<TimelineItem, { kind: "tool" }> =>
    item.kind === "tool" &&
    !item.depth &&
    item.call.name !== "AskUserQuestion" &&
    item.call.result !== null &&
    !item.call.result.isError &&
    !gated.has(item.call.toolUseId) &&
    !neverFolded(item.call);
  const absorbable = (item: TimelineItem) => absorbThoughts && item.kind === "thinking" && !item.open;
  // A run that reaches past the last item other than thinking may still grow while the turn runs.
  let lastNonThinking = -1;
  for (let k = items.length - 1; k >= 0; k--) {
    if (items[k].kind !== "thinking") {
      lastNonThinking = k;
      break;
    }
  }
  const out: TimelineItem[] = [];
  let i = 0;
  while (i < items.length) {
    const start = i;
    // `end` stays just after the last call taken, so thoughts after the last call are given back.
    let end = i;
    let calls = 0;
    let turn: string | undefined;
    for (let j = i; j < items.length; j++) {
      const item = items[j];
      const itemTurn = turnOf(item);
      if (calls > 0 && turn !== undefined && itemTurn !== undefined && itemTurn !== turn) break;
      if (foldable(item)) {
        if (turn === undefined) turn = itemTurn;
        calls++;
        end = j + 1;
      } else if (!(calls > 0 && absorbable(item))) {
        break;
      }
    }
    i = end;
    const atEnd = end > lastNonThinking;
    const first = items[start];
    const key = first?.kind === "tool" ? `r-${first.seq}` : "";
    if (calls >= 2 && !(atEnd && opts.turnRunning) && !opts.expanded[key]) {
      out.push(runOf(items.slice(start, end)));
    } else {
      out.push(...items.slice(start, end));
    }
    if (i < items.length && i === start) {
      out.push(items[i]);
      i++;
    }
  }
  return out;
}

/** The turn a call or thought belongs to, or `undefined` when it names none (or is another kind). */
function turnOf(item: TimelineItem): string | undefined {
  const turn = item.kind === "tool" ? item.call.turnId : item.kind === "thinking" ? item.block.turnId : undefined;
  return turn === "" ? undefined : turn;
}

/** Each run item, by its first member, with the members it was made from: the same members give back
 *  the same item, so a run row is not drawn again while something else in the conversation streams
 *  (rows are memoized on their item, and `buildTimeline` keeps a record's item while the record is the
 *  same). */
const runItems = new WeakMap<TimelineItem, { members: TimelineItem[]; run: TimelineItem }>();

function runOf(members: TimelineItem[]): TimelineItem {
  const first = members[0];
  const held = runItems.get(first);
  if (held !== undefined && held.members.length === members.length && held.members.every((m, i) => m === members[i])) {
    return held.run;
  }
  const calls = members.flatMap((item) => (item.kind === "tool" ? [item.call] : []));
  const thoughts = members.flatMap((item) => (item.kind === "thinking" ? [item.block] : []));
  const run: TimelineItem = { kind: "run", seq: first.seq, key: `r-${first.seq}`, calls, ...(thoughts.length > 0 ? { thoughts } : {}) };
  runItems.set(first, { members, run });
  return run;
}

/** The sequence of a call, thought or reply, including a row inside a delegation. */
function memberSeq(key: string): number {
  return key.startsWith("t-") || key.startsWith("k-") || key.startsWith("m-") ? Number(key.slice(2)) : NaN;
}

/** Whether `item` is a run holding the call or thought numbered `seq`. */
function runHolds(item: TimelineItem, seq: number): boolean {
  return (
    item.kind === "run" &&
    (item.calls.some((c) => c.seq === seq) || (item.thoughts ?? []).some((t: ThinkingBlock) => t.seq === seq))
  );
}

/** Where `key` is now -- a `t-<seq>` or `k-<seq>` folded into a run is found at the run. */
export function indexOfKey(items: TimelineItem[], key: string): number | null {
  const direct = items.findIndex((item) => item.key === key);
  if (direct !== -1) return direct;
  const seq = memberSeq(key);
  const inRun = items.findIndex((item) => runHolds(item, seq));
  if (inRun !== -1) return inRun;
  const parent = items.findIndex((item) => item.kind === "tool" && holdsChild(item.call, seq));
  return parent === -1 ? null : parent;
}

/** `zc` closes the run a row came from, or its innermost expanded delegation. */
export function runKeyOf(base: TimelineItem[], opts: Options, rowKey: string): string | null {
  const seq = memberSeq(rowKey);
  const run = buildDisplay(base, { ...opts, expanded: {} }).find((it) => runHolds(it, seq));
  if (run) return run.key;
  const owner = (items: TimelineItem[]): string | null => {
    for (const item of items) {
      if (item.kind !== "tool" || !item.call.children || isBackgroundAgent(item.call)) continue;
      const nested = owner(item.call.children.toolCalls.map((call) => ({ kind: "tool", seq: call.seq, key: `t-${call.seq}`, call })));
      if (nested) return nested;
      if (opts.expanded[item.key] && holdsChild(item.call, seq)) return item.key;
    }
    return null;
  };
  return owner(base);
}

const field = (value: unknown, key: string): unknown =>
  value !== null && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined;
const word = (value: unknown, key: string): string => typeof field(value, key) === "string" ? field(value, key) as string : "";

function resultText(call: ToolCallRecord): string {
  const content = call.result?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) return content.map((b) => word(b, "text")).join("\n");
  return "";
}
function resultObject(call: ToolCallRecord): unknown {
  const content = call.result?.content;
  if (content !== null && typeof content === "object" && !Array.isArray(content)) return content;
  try { return JSON.parse(resultText(call)); } catch { return null; }
}
const durationText = (ms: number): string => {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`;
};
function clockText(value: unknown): string {
  if (typeof value !== "number" && typeof value !== "string") return "";
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" }) : "";
}

type CardState = Pick<AgentUiState, "backgroundTasks" | "notices" | "turnStarts" | "activeTurnId">;

const CARD_LINE_TOOLS = new Set([
  "Bash", "Monitor", "Agent", "Task", "Workflow", "RunWorkflow", "TaskStop", "ScheduleWakeup", "CronCreate",
  "CronDelete", "CronList", "SendMessage", "ListAgents",
]);

/** Whether the roster, a notice or a turn's cause already names this very call. */
function mentionsToolUse(call: ToolCallRecord, state?: CardState): boolean {
  if (!state) return false;
  const id = call.toolUseId;
  return state.backgroundTasks.rows.some((r) => r.tool_use_id === id) ||
    state.notices.some((n) => n.kind === "task" && n.toolUseId === id) ||
    state.turnStarts.some((t) => t.cause?.kind === "task" && t.cause.toolUseId === id);
}

/** Background launch results describe the past. Only the roster can establish that a task is live. */
export function toolCardLine(call: ToolCallRecord, state?: CardState): string | null {
  if (!CARD_LINE_TOOLS.has(call.name)) return null;
  const input = call.input, text = resultText(call);
  const workflow = call.name === "Workflow" || call.name === "RunWorkflow";
  // A foreground Bash/Monitor/Agent/Task call is the common case and is not a background card: leave it before
  // the result is parsed, since this runs for every such call on every render.
  if (["Bash", "Monitor", "Agent", "Task"].includes(call.name) && call.background !== true &&
      field(input, "run_in_background") !== true && !mentionsToolUse(call, state) &&
      !(BASH_BACKGROUND_LAUNCH.test(text) || (call.name !== "Bash" && /task_id|taskId|^Task ID:/m.test(text)))) {
    return null;
  }
  const result = resultObject(call);
  const taskId = word(result, "task_id") || word(result, "taskId") ||
    text.match(/^(?:Command running in background with ID:|Command [^\n]{1,80}?\(ID:)\s*([\w-]+)/)?.[1] ||
    (call.name !== "Bash" ? text.match(/^Task ID:\s*([\w-]+)/m)?.[1] : "") || "";
  const task = state?.backgroundTasks.rows.find((r) => r.tool_use_id === call.toolUseId || (!!taskId && r.task_id === taskId));
  const notice = state?.notices.slice().reverse().find((n) => n.kind === "task" &&
    (n.toolUseId === call.toolUseId || (!!taskId && n.taskId === taskId)) && ["completed", "stopped", "failed"].includes(n.taskStatus ?? ""));
  const cause = state?.turnStarts.slice().reverse().map((t) => t.cause).find((c) => c?.kind === "task" &&
    (c.toolUseId === call.toolUseId || (!!taskId && c.taskId === taskId)) && ["completed", "stopped", "failed"].includes(c.taskStatus ?? ""));
  // The CLI follows a foreground command as a task too, from a few seconds in, so that it can be moved aside, and
  // that task says it is not in the background. Until it is moved (its task then says so and its result names the
  // move), the command runs in its turn like any other, and its own row says so and then shows what it printed.
  if (call.name === "Bash" && task?.is_backgrounded === false && call.background !== true &&
      field(input, "run_in_background") !== true && !BASH_BACKGROUND_LAUNCH.test(text)) {
    return null;
  }
  if (call.name === "TaskStop") {
    const target = state?.backgroundTasks.rows.find((r) => r.task_id === word(input, "task_id"));
    return `TaskStop · "${target?.description || word(input, "task_id")}"`;
  }
  if (workflow || (["Bash", "Monitor", "Agent", "Task"].includes(call.name) &&
      (task || notice || cause || call.background || field(input, "run_in_background") === true || taskId))) {
    if (!task && !notice && !cause && call.result === null && state?.activeTurnId &&
        (!call.turnId || call.turnId === state.activeTurnId)) return null;
    const name = task?.workflow_name || word(input, "name") || word(input, "workflow_name") ||
      word(input, "description") || word(input, "summary") || word(result, "workflow_name") ||
      text.match(/^Summary: (.+)$/m)?.[1] || "";
    const prefix = workflow ? `Workflow${name ? ` ${name}` : ""} · ` : "⎿ ";
    const launchError = word(result, "error");
    // An error result is drawn under the card by the ordinary result row (a workflow's is not), so the
    // line says only that it failed rather than printing the same text twice.
    if (launchError) return `${prefix}failed: ${launchError}`;
    if (call.result?.isError) return workflow ? `${prefix}failed: ${text}` : `${prefix}failed`;
    if (task?.live) return `${prefix}${workflow ? "running" : "Running"} in the background · /tasks`;
    const status = task && ["completed", "failed", "stopped", "ended"].includes(task.status) ? task.status : notice?.taskStatus ?? cause?.taskStatus ?? task?.status;
    const summary = task?.summary || notice?.content || cause?.summary || "";
    if (workflow && summary && ["completed", "failed", "stopped"].includes(status ?? "")) return summary;
    let label = "stopped with the session";
    if (summary.includes("was stopped after reaching its background time limit")) label = "stopped at its background time limit";
    else if (/low on memory/i.test(summary)) label = "stopped: the system was low on memory";
    else if (status === "completed") label = "completed in background";
    else if (status === "stopped") label = "stopped";
    else if (status === "failed") {
      const code = summary.match(/exit code[ :]+(-?\d+)/i)?.[1];
      label = `failed${code ? ` · exit code ${code}` : ""}`;
    }
    const duration = task?.usage?.duration_ms ?? (task?.ended_at !== null && task?.ended_at !== undefined ? task.ended_at - task.started_at : null);
    return `${prefix}${label}${duration === null ? "" : ` · ${durationText(duration)}`}`;
  }
  if (call.name === "ScheduleWakeup") {
    const seconds = field(input, "delaySeconds");
    const delay = typeof seconds === "number" ? (seconds % 60 === 0 ? `${seconds / 60}m` : `${seconds}s`) : "?";
    const reason = word(input, "reason").split("<<autonomous-loop-dynamic>>").join("");
    const time = clockText(field(result, "scheduledFor")) || text.match(/scheduled for (\d{2}:\d{2})/)?.[1];
    const status = word(result, "status");
    const outcome = status === "stopped" || status === "no-op" ? status : field(result, "noop") === true || field(input, "noop") === true ? "no-op" : time ? `at ${time}` : "";
    return `Wake up in ${delay}: ${reason}${outcome ? ` · ${outcome}` : ""}`;
  }
  if (call.name === "CronCreate") {
    const schedule = word(result, "humanSchedule") || word(input, "cron");
    const next = clockText(field(result, "nextFireAt")) || text.match(/Next run: ([^.]+)/)?.[1];
    return `${schedule}: ${word(input, "prompt").split("<<autonomous-loop-dynamic>>").join("")}${next ? ` · next run ${next}` : ""}`;
  }
  if (call.name === "CronDelete") return `CronDelete ${word(input, "id")}`;
  if (call.name === "CronList") return "list scheduled tasks";
  if (call.name === "SendMessage") return `→ ${word(input, "to")}: ${(word(input, "message") || word(input, "content") || word(input, "text")).split(/\r?\n/, 1)[0]}`;
  if (call.name === "ListAgents") return "list agents";
  return null;
}

export const isAgentCall = (call: ToolCallRecord): boolean =>
  call.name === "Agent" || call.name === "Task" || call.children !== undefined;
export const isBackgroundAgent = (call: ToolCallRecord): boolean =>
  call.background === true || field(call.input, "run_in_background") === true ||
  (call.result === null && (call.children?.toolCalls.some((child) => child.background === true) ?? false)) ||
  field(resultObject(call), "isAsync") === true ||
  /(?:was moved to the background|Task ID:|isAsync["\s:]+true)/.test(resultText(call));

/** Child calls stay in their own tree; counts and the newest call include nested delegations. */
function descendantCalls(call: ToolCallRecord): ToolCallRecord[] {
  return (call.children?.toolCalls ?? []).flatMap((child) => [child, ...descendantCalls(child)]);
}

type AgentCard = { header: string; line: string; background: boolean; warnings: string[]; report: string };
const agentCards = new WeakMap<ToolCallRecord, AgentCard>();

/** `abandoned` is the conversation's verdict that this call's result can never arrive (its turn is over,
 * the session ended, or history restored it without one); only the caller knows whether the session ended. */
export function agentCard(call: ToolCallRecord, state?: CardState, abandoned = false): AgentCard {
  const calls = descendantCalls(call);
  const text = resultText(call);
  const result = resultObject(call);
  const task = state?.backgroundTasks.rows.find((row) => row.tool_use_id === call.toolUseId);
  const usage = task?.usage;
  const metric = (key: string, pattern: string): number | undefined => {
    const value = field(result, key);
    if (typeof value === "number" && Number.isFinite(value) && value >= 0) return value;
    const match = text.match(new RegExp(`${pattern}:\\s*(\\d+)`));
    const parsed = match ? Number(match[1]) : NaN;
    return Number.isFinite(parsed) ? parsed : undefined;
  };
  // The completed result includes work done after the last progress update.
  const count = metric("totalToolUseCount", "tool_uses") ?? usage?.tool_uses ?? calls.length;
  const tokens = metric("totalTokens", "(?:total|subagent)_tokens") ?? usage?.total_tokens;
  const duration = metric("totalDurationMs", "duration_ms") ?? usage?.duration_ms;
  const metrics = [`${count} tool uses`, ...(typeof tokens === "number" && tokens > 0
    ? [`${tokens >= 1000 ? `${Math.round(tokens / 1000)}k` : tokens} tokens`] : []),
    ...(typeof duration === "number" ? [durationText(duration)] : [])].join(" · ");
  const last = calls.reduce<ToolCallRecord | null>((last, child) => !last || child.seq > last.seq ? child : last, null);
  const oneLine = (value: string) => value.split(/\r?\n/, 1)[0];
  const lastInput = last && (word(last.input, "command") || word(last.input, "file_path") || word(last.input, "pattern") || word(last.input, "description"));
  const lastLine = last ? `${shortToolName(last.name)}${lastInput ? `(${oneLine(lastInput)})` : ""}` : task?.last_tool_name || "Running";
  const background = isBackgroundAgent(call) || task?.is_backgrounded === true;
  const liveLine = `⎿ ${lastLine} · ${count} tool uses${typeof duration === "number" ? ` · ${durationText(duration)}` : ""}`;
  // A launch still in flight has no card line yet: until the roster lists its task, the card cannot
  // say it runs in the background (nor open it in /tasks), so it reads like a foreground agent's live
  // line. "stopped with the session" is for a call that ended without a result. The stand-in parent of
  // frames whose parent is unknown has no turn and is never in flight.
  const inFlight = call.result === null && !!state?.activeTurnId && call.turnId === state.activeTurnId;
  const settled = background ? toolCardLine(call, state) : null;
  const line = background ? settled !== null ? settled.replace(" · /tasks", ` · ${count} tool uses · /tasks`)
      : inFlight ? liveLine : "⎿ stopped with the session"
    : call.result ? `⎿ ${call.result.isError ? "Failed" : "Done"} (${metrics})`
    // A foreground agent that ended without a result says so, as any other call does, instead of
    // keeping its live line for good.
    : abandoned ? `⎿ no result recorded (${metrics})`
    : liveLine;
  const report = (text || word(result, "result") || word(result, "report")).replace(/<usage>[\s\S]*?<\/usage>/g, "").trim();
  const warnings: string[] = [];
  // The CLI puts its fallback and turn-limit notes at the head of the report; a later line that merely
  // mentions turns or a model belongs to the report itself.
  const body = report.split("\n").filter((line, index) => {
    if (index < 2 && /switched to .+ because|turn.limit|maximum (?:number of )?turns|max.?turns|model.*fallback/i.test(line)) {
      warnings.push(line); return false;
    }
    return true;
  }).join("\n");
  const card = { header: `${word(call.input, "subagent_type") || task?.subagent_type || "Agent"}(${word(call.input, "description") || task?.description || ""})`,
    line, background, warnings, report: body };
  const held = agentCards.get(call);
  if (held && held.header === card.header && held.line === line && held.background === background &&
      held.report === body && held.warnings.length === warnings.length && held.warnings.every((warning, i) => warning === warnings[i])) return held;
  agentCards.set(call, card);
  return card;
}

function holdsChild(call: ToolCallRecord, seq: number): boolean {
  const stream = call.children;
  return !!stream && (stream.toolCalls.some((child) => child.seq === seq || holdsChild(child, seq)) ||
    stream.transcript.some((message) => message.seq === seq) || stream.thinking.some((block) => block.seq === seq));
}
