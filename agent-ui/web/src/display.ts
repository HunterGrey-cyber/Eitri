import type { TimelineItem } from "./timeline";
import type { ToolCallRecord } from "./types";

type Options = { expanded: Record<string, boolean>; detailed: boolean; turnRunning: boolean };

/** `Read ×3 · Bash ×2`, names in first-seen order (Claude Code's `Called slack 3 times`). */
export function runSummary(calls: ToolCallRecord[]): string {
  const counts = new Map<string, number>();
  for (const c of calls) counts.set(c.name, (counts.get(c.name) ?? 0) + 1);
  return Array.from(counts, ([name, n]) => `${name} ×${n}`).join(" · ");
}

/** P2 (ruling 21): consecutive finished, ungated tool calls -- two or more, with something after
 *  them or a turn that is over -- become one `run` row. Expanded runs and the detailed view keep
 *  every call.
 *
 * A failed call never joins a run (sw-panel-render-5): every run row draws a fixed `✓` sign and
 * `runSummary` names only counts, so a failing call folded in among successes vanished behind a
 * success sign until the reader expanded the row. Excluding it from the loop below ends the run at
 * the failure -- the failing call falls out to the single-item path just below, and its own row
 * draws `✗` exactly as an unfolded failed call always has (`MessageList.tsx`'s `toolSign`). */
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
      if (item.kind !== "tool" || item.call.result === null || item.call.result.isError || gated.has(item.call.toolUseId)) break;
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
