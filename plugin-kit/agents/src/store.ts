export type Project = { key: string; name: string };
export type Row = {
  id: string;
  key: string;
  agent: string;
  project: Project;
  window: { instance: string; name: string; focused: boolean };
  number: number;
  label: string;
  label_from: string;
  shown: boolean;
  state: string;
  class: string;
  since: number;
  needs: { cards: number; questions: number; trust: boolean };
  waiting_since: number | null;
  unread: boolean;
  tool: string | null;
  background: { calls: number; shells: number; monitors: number; agents: number; other: number };
  queued: number;
  mode: string;
  ended_how: string | null;
  moving: boolean;
};
export type Selection = { key: string; at: number; by?: string };
export type Round = {
  current: string | null;
  visited: string[];
  started_at: number;
  at: number;
  from: { instance: string; module: string; tab: number | null };
};
export type View = {
  v: number;
  at: number;
  enabled: boolean;
  projects: Project[];
  rows: Row[];
  selection: Selection | null;
  round: Round | null;
};
export type Store = {
  rows(): Row[];
  byKey(key: string): Row | undefined;
  selection(): string | null;
  round(): Round | null;
  on(fn: () => void): () => void;
};
export type Bridge = {
  call(verb: string, args: unknown): Promise<unknown>;
  on(event: string, fn: (data: unknown) => void): () => void;
};

// Copies keep a face's sorting and decoration from changing the shared view.
function copy<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}
const compare = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;
function viewOrder(a: Row, b: Row): number {
  return compare(a.project.name, b.project.name) || compare(a.project.key, b.project.key) ||
    compare(a.window.name, b.window.name) || compare(a.window.instance, b.window.instance) || a.number - b.number;
}

export function createStore() {
  let rows = new Map<string, Row>();
  let selection: string | null = null;
  let round: Round | null = null;
  const listeners = new Set<() => void>();
  const store: Store = Object.freeze({
    rows: () => [...rows.values()].sort(viewOrder).map(copy),
    byKey: (key: string) => { const row = rows.get(key); return row && copy(row); },
    selection: () => selection,
    round: () => copy(round),
    on(fn: () => void) { listeners.add(fn); return () => { listeners.delete(fn); }; },
  });
  function apply(event: string, data: unknown): void {
    switch (event) {
      case "agents.row": {
        const row = copy(data as Row);
        rows.set(row.key, row);
        break;
      }
      case "agents.gone": {
        const gone = data as { key: string; id: string };
        if (rows.get(gone.key)?.id !== gone.id) return;
        rows.delete(gone.key);
        break;
      }
      case "agents.reset": {
        const view = copy(data as View);
        rows = new Map(view.rows.map(row => [row.key, row]));
        selection = view.selection?.key ?? null;
        round = view.round;
        break;
      }
      case "agents.selection": selection = (data as Selection | null)?.key ?? null; break;
      case "agents.round": round = copy(data as Round | null); break;
      default: return;
    }
    for (const fn of [...listeners]) {
      try { fn(); } catch (error) { console.error(error); }
    }
  }
  return { store, apply };
}
