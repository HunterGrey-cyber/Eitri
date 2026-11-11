import type { AgentUiSnapshot } from "./types";
import { applySnapshot, initialState } from "./reducer";
import { buildTimeline, type TimelineItem } from "./timeline";
import { buildDisplay, runKeyOf } from "./display";

export type HistoryPage = { fromTurn: number; records: AgentUiSnapshot[] };
export type HistoryEnvelope = { kind: "history_page"; tab: number } &
  ({ cleared: true } | (HistoryPage & { requestId: string }));

function historyState(snapshot: AgentUiSnapshot) {
  const state = applySnapshot(initialState(), snapshot, 0);
  state.pendingPermissions = [];
  state.editorCards = [];
  state.questions = state.questions.filter((q) => q.state.kind === "settled");
  state.activeTurnId = null;
  return state;
}

const historyExpanded = (expanded: Record<string, boolean>) =>
  Object.fromEntries(Object.entries(expanded).filter(([key]) => key.startsWith("h:")).map(([key, value]) => [key.slice(2), value]));

/** Resolve a historical fold within its own turn, apart from live sequence numbers. */
export function historyRunKeyOf(page: HistoryPage | null, options: Parameters<typeof buildDisplay>[1], key: string): string | null {
  if (page === null || !key.startsWith("h:")) return null;
  const historical = { ...options, expanded: historyExpanded(options.expanded), turnRunning: false };
  for (const snapshot of page.records) {
    const run = runKeyOf(buildTimeline(historyState(snapshot)), historical, key.slice(2));
    if (run !== null) return `h:${run}`;
  }
  return null;
}

/** Historical records are a separate reading surface. No card or live-session state can escape it. */
export function historyTimeline(page: HistoryPage | null, options: Parameters<typeof buildDisplay>[1]): TimelineItem[] {
  if (page === null) return [];
  const expanded = historyExpanded(options.expanded);
  return page.records.flatMap((snapshot) => {
    const state = historyState(snapshot);
    return buildDisplay(buildTimeline(state), { ...options, expanded, turnRunning: false })
      .filter((row) => row.kind !== "permission")
      .map((row) => ({ ...row, key: `h:${row.key}` }));
  });
}
