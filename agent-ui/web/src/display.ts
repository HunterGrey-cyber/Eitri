import type { TimelineItem } from "./timeline";
import type { ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "./types";
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

/** `Write`, `Edit` and `NotebookEdit` can finish without a permission card under `acceptEdits`.
 *  Keep each change in its own row so it cannot disappear into a tool-count summary. These tools
 *  end a run on either side, just as failed or gated calls do; reads and searches still fold. */
const NEVER_FOLDED_TOOLS = new Set(["Write", "Edit", "NotebookEdit", "fileChange"]);

/** The same for an ACP agent's file-changing kinds: the change is its own row, not a count in a run. */
const neverFolded = (call: ToolCallRecord): boolean =>
  NEVER_FOLDED_TOOLS.has(call.name) ||
  /^acp:(edit|delete|move)$/.test(call.name) ||
  // A subagent's call keeps its own row, so whose it is stays on screen instead of inside a count.
  subagentOf(call.name, call.input) !== null;

/** Fold consecutive finished, ungated tool calls into one `run` row when there are at least
 *  two and another item follows or the turn has ended. Expanded runs and the detailed view keep
 *  every call.
 *
 * Failed calls end a run and keep their own `✗` row. A run's fixed `✓` sign and count summary
 * would otherwise hide the failure until the row was expanded.
 *
 * Under `hidden`, remove thinking before folding. Under `collapsed` and `preview`, fold finished
 * thoughts between calls into the run so a thought before every call does not break it up.
 * Expanding the run restores those thoughts as separate rows. Thoughts before the first call,
 * after the last call, or still being written keep their own rows. Under `expanded`, thoughts
 * never fold. The detailed view keeps everything at every thinking level.
 *
 * Runs stop at a call or thought from another turn so each turn's "Worked for" line can follow
 * its last row. An item without a turn can join either side. */
export function buildDisplay(timeline: TimelineItem[], opts: Options): TimelineItem[] {
  if (opts.detailed) return timeline;
  const level = opts.thinking ?? "collapsed";
  const items = level === "hidden" ? timeline.filter((item) => item.kind !== "thinking") : timeline;
  const absorbThoughts = level === "collapsed" || level === "preview";
  const gated = new Set(
    items.flatMap((item) => (item.kind === "permission" && item.request.toolUseId ? [item.request.toolUseId] : [])),
  );
  const foldable = (item: TimelineItem): item is Extract<TimelineItem, { kind: "tool" }> =>
    item.kind === "tool" &&
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

/** The `seq` a row key names, for the two kinds a run can hold: `t-<seq>` (a call) and `k-<seq>` (a
 *  thought). `NaN` for any other key. */
function memberSeq(key: string): number {
  return key.startsWith("t-") || key.startsWith("k-") ? Number(key.slice(2)) : NaN;
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
  return inRun === -1 ? null : inRun;
}

/** `zc`: the run a `t-<seq>` or `k-<seq>` row was unfolded from -- `buildDisplay` keys a run
 *  by its first call, so the same timeline folded with nothing expanded finds it. `null` for a row in
 *  no run. */
export function runKeyOf(base: TimelineItem[], opts: Options, rowKey: string): string | null {
  const seq = memberSeq(rowKey);
  const run = buildDisplay(base, { ...opts, expanded: {} }).find((it) => runHolds(it, seq));
  return run?.key ?? null;
}
