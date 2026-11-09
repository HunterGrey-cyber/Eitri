import { allCards } from "./timeline";
import type { TimelineItem } from "./timeline";
import type { AgentUiState, Seq, TurnTimeWire } from "./types";

/** How long one finished turn ran, and which row it finished on. */
export type TurnDuration = { durationMs: number; anchorSeq: Seq | null };

/** The active tab's finished turns by turn id. Replaced whole by a snapshot's list and extended by an
 *  events envelope's, so a turn Rust never stamped (history seeded by a resume, a turn that began before
 *  this process) simply has no entry and shows no time. */
export type TurnDurations = ReadonlyMap<string, TurnDuration>;

export const NO_TURN_DURATIONS: TurnDurations = new Map();

function entry(time: TurnTimeWire): [string, TurnDuration] {
  return [time.turnId, { durationMs: Math.max(0, time.endedAtMs - time.startedAtMs), anchorSeq: time.anchorSeq ?? null }];
}

/** A snapshot's `turnTimes` as the whole table. A sender that predates the field reads as no times. */
export function durationsFromSnapshot(times: readonly TurnTimeWire[] | undefined): TurnDurations {
  return new Map((times ?? []).map(entry));
}

/** An events envelope's `turnTimes` added to what the panel holds; `current` itself when it carries none. */
export function durationsWith(current: TurnDurations, times: readonly TurnTimeWire[] | undefined): TurnDurations {
  if (times === undefined || times.length === 0) return current;
  return new Map([...current, ...times.map(entry)]);
}

/** `8s`, `1m 23s`, `1h 02m`: seconds under a minute, minutes with zero-padded seconds under an hour,
 *  hours with zero-padded minutes after that. Whole units, rounded down. */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m ${String(total % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/** The "Worked for" lines to draw, by the `seq` of the row each stands under (Rust's anchor: the newest
 *  row the agent wrote, a thought included; `placeFooters` finds where that row is drawn). Left out: a
 *  turn that is still running, a turn that ended badly (its ending row says how long instead), a turn
 *  whose row Rust could not name or that is not in this state at all, and the second of two turns naming
 *  the same row (it produced none of its own, so the line would stand under the wrong turn's work). */
export function workedFooters(
  state: Pick<AgentUiState, "activeTurnId" | "turnEndings" | "transcript" | "thinking" | "toolCalls" | "pendingPermissions"> &
    Partial<Pick<AgentUiState, "editorCards">>,
  durations: TurnDurations,
): ReadonlyMap<Seq, string> {
  if (durations.size === 0) return new Map();
  const ended = new Set(state.turnEndings.map((ending) => ending.turnId));
  // The rows Rust's anchor can name (`newest_item_seq`): everything the agent wrote, never a prompt.
  const rows = new Set<Seq>();
  for (const list of [state.transcript, state.thinking, state.toolCalls, allCards(state), state.turnEndings])
    for (const item of list) rows.add(item.seq);
  const out = new Map<Seq, string>();
  for (const [turnId, { durationMs, anchorSeq }] of durations) {
    if (anchorSeq === null || !rows.has(anchorSeq)) continue;
    if (turnId === state.activeTurnId || ended.has(turnId) || out.has(anchorSeq)) continue;
    out.set(anchorSeq, `Worked for ${formatDuration(durationMs)}`);
  }
  return out;
}

/** A broker card's `seq` is the revision the next event would take, so a later reply may share it; Rust's anchor never
 *  names a card, and a footer never stands under one. */
function isEditorCardItem(item: TimelineItem): boolean {
  return item.kind === "permission" && item.request.editorCard !== undefined;
}

/** Where each "Worked for" line stands on the page actually drawn: by the `key` of the row it goes under.
 *
 * Rust names the newest row a turn wrote (`anchorSeq`), and that row is not always drawn: a thought under
 * `agent.thinking = "hidden"`, or one folded into a run of calls. So the line goes under the drawn row or run
 * holding the anchor; failing that, under the last drawn row at or before it, which is the turn's newest row
 * that is on screen -- never past a later prompt, since that row would be the next turn's. A turn with
 * nothing drawn at or before its anchor gets no line. Two lines never share a row: a line placed on its own
 * anchor keeps the row, and otherwise the older turn's, the one the row belongs to. */
export function placeFooters(items: readonly TimelineItem[], footers: ReadonlyMap<Seq, string>): ReadonlyMap<string, string> {
  const out = new Map<string, string>();
  if (footers.size === 0) return out;
  const anchors = [...footers.keys()].sort((a, b) => a - b);
  // Each row by every `seq` it holds, the first row to hold one: a turn's line is looked up once per turn, and
  // walking the rows for each of them is a turn count times a row count on every streamed batch.
  const holders = new Map<Seq, TimelineItem>();
  const fallback: { seq: Seq; item: TimelineItem }[] = [];
  let promptSeq = -Infinity;
  const hold = (seq: Seq, item: TimelineItem) => {
    if (!holders.has(seq)) holders.set(seq, item);
  };
  for (const item of items) {
    if (isEditorCardItem(item)) continue;
    if (item.kind === "prompt") promptSeq = Math.max(promptSeq, item.seq);
    // A row is reachable only past every preceding prompt and its own sequence. A later row
    // with a lower threshold supersedes earlier candidates, even when display order differs from seq.
    const seq = Math.max(promptSeq, item.seq);
    while (fallback.length > 0 && fallback[fallback.length - 1].seq >= seq) fallback.pop();
    fallback.push({ seq, item });
    if (item.kind !== "run") {
      hold(item.seq, item);
      continue;
    }
    for (const call of item.calls) hold(call.seq, item);
    for (const thought of item.thoughts ?? []) hold(thought.seq, item);
  }
  const left: Seq[] = [];
  for (const anchor of anchors) {
    const at = holders.get(anchor);
    if (at === undefined) left.push(anchor);
    else if (!out.has(at.key)) out.set(at.key, footers.get(anchor)!);
  }
  for (const anchor of left) {
    let low = 0, high = fallback.length;
    while (low < high) {
      const mid = Math.floor((low + high) / 2);
      if (fallback[mid].seq <= anchor) low = mid + 1;
      else high = mid;
    }
    const under = fallback[low - 1]?.item;
    if (under !== undefined && !out.has(under.key)) out.set(under.key, footers.get(anchor)!);
  }
  return out;
}

/** How long a turn that ended badly ran, worded for its ending row; `null` when unknown. */
export function endingDuration(durations: TurnDurations, turnId: string): string | null {
  const known = durations.get(turnId);
  return known === undefined ? null : formatDuration(known.durationMs);
}
