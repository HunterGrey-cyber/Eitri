import type { TimelineItem } from "./timeline";
import type { ToolCallRecord } from "./types";

type Options = { expanded: Record<string, boolean>; detailed: boolean; turnRunning: boolean };

/** `Read ×3 · Bash ×2`, names in first-seen order (Claude Code's `Called slack 3 times`). */
export function runSummary(calls: ToolCallRecord[]): string {
  const counts = new Map<string, number>();
  for (const c of calls) counts.set(c.name, (counts.get(c.name) ?? 0) + 1);
  return Array.from(counts, ([name, n]) => `${name} ×${n}`).join(" · ");
}

/** `Write`, `Edit` and `NotebookEdit` can finish without a permission card under `acceptEdits`.
 *  Keep each change in its own row so it cannot disappear into a tool-count summary. These tools
 *  end a run on either side, just as failed or gated calls do; reads and searches still fold. */
const NEVER_FOLDED_TOOLS = new Set(["Write", "Edit", "NotebookEdit"]);

/** Fold consecutive finished, ungated tool calls into one `run` row when there are at least
 *  two and another item follows or the turn has ended. Expanded runs and the detailed view keep
 *  every call.
 *
 * Failed calls end a run and keep their own `✗` row. A run's fixed `✓` sign and count summary
 * would otherwise hide the failure until the row was expanded. */
export function buildDisplay(timeline: TimelineItem[], opts: Options): TimelineItem[] {
  if (opts.detailed) return timeline;
  const gated = new Set(
    timeline.flatMap((item) => (item.kind === "permission" && item.request.toolUseId ? [item.request.toolUseId] : [])),
  );
  const out: TimelineItem[] = [];
  let i = 0;
  while (i < timeline.length) {
    const start = i;
    while (i < timeline.length) {
      const item = timeline[i];
      if (
        item.kind !== "tool" ||
        item.call.result === null ||
        item.call.result.isError ||
        gated.has(item.call.toolUseId) ||
        NEVER_FOLDED_TOOLS.has(item.call.name)
      )
        break;
      i++;
    }
    const length = i - start;
    const atEnd = i === timeline.length;
    const first = timeline[start];
    const key = first?.kind === "tool" ? `r-${first.seq}` : "";
    if (length >= 2 && !(atEnd && opts.turnRunning) && !opts.expanded[key]) {
      const calls = timeline.slice(start, i).map((item) => (item as Extract<TimelineItem, { kind: "tool" }>).call);
      out.push({ kind: "run", seq: first.seq, key, calls });
    } else {
      out.push(...timeline.slice(start, i));
    }
    if (i < timeline.length && i === start) {
      out.push(timeline[i]);
      i++;
    }
  }
  return out;
}

/** Where `key` is now -- a `t-<seq>` folded into a run is found at the run. */
export function indexOfKey(items: TimelineItem[], key: string): number | null {
  const direct = items.findIndex((item) => item.key === key);
  if (direct !== -1) return direct;
  const seq = key.startsWith("t-") ? Number(key.slice(2)) : NaN;
  const inRun = items.findIndex((item) => item.kind === "run" && item.calls.some((c) => c.seq === seq));
  return inRun === -1 ? null : inRun;
}

/** `zc`: the run a `t-<seq>` row was unfolded from -- `buildDisplay` keys a run by its
 *  first call, so the same timeline folded with nothing expanded finds it. `null` for a row in no run. */
export function runKeyOf(base: TimelineItem[], opts: Options, rowKey: string): string | null {
  const seq = rowKey.startsWith("t-") ? Number(rowKey.slice(2)) : NaN;
  const run = buildDisplay(base, { ...opts, expanded: {} }).find(
    (it) => it.kind === "run" && it.calls.some((c) => c.seq === seq),
  );
  return run?.key ?? null;
}
