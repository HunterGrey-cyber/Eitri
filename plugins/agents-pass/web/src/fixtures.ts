// Test helpers: rows shaped as the agents kit hands them, and a store that changes on demand.
import type { PassRound, PassRow, PassStore } from "./strip";

const CLASS: Record<string, string> = {
  card: "needs", question: "needs", trust: "needs", running: "working", background: "working",
  starting: "working", idle: "quiet", done: "quiet", new: "quiet", failed: "ended", ended: "ended",
};

export function row(key: string, state: string, more: Partial<PassRow> = {}): PassRow {
  const cls = CLASS[state] ?? "quiet";
  return {
    id: `id-${key}`,
    key,
    agent: "claude",
    project: { key: "p-eitri", name: "eitri" },
    window: { instance: "w1", name: "1" },
    number: 1,
    label: `label ${key}`,
    state,
    class: cls,
    since: 1_000,
    needs: { cards: state === "card" ? 1 : 0, questions: state === "question" ? 1 : 0, trust: state === "trust" },
    waiting_since: cls === "needs" ? 1_000 : null,
    ...more,
  };
}

export function round(current: string | null, visited: string[]): PassRound {
  return { current, visited, started_at: 0, at: 0, from: { instance: "w1", module: "agent", tab: null } };
}

export function fakeStore(rows: PassRow[], r: PassRound | null = null, selection: string | null = null) {
  const state = { rows, round: r, selection };
  const listeners = new Set<() => void>();
  const store: PassStore = {
    rows: () => state.rows,
    byKey: (key) => state.rows.find((x) => x.key === key),
    selection: () => state.selection,
    round: () => state.round,
    on(fn) { listeners.add(fn); return () => { listeners.delete(fn); }; },
  };
  return {
    store,
    set(patch: Partial<typeof state>) {
      Object.assign(state, patch);
      for (const fn of [...listeners]) fn();
    },
  };
}

const GLYPHS: Record<string, string> = { card: "⚑", question: "?", trust: "?", running: "●", idle: "·", done: "•" };
export const glyph = (r: PassRow) => GLYPHS[r.state] ?? (r.class === "needs" ? "⚑" : "·");
