// The pass: one line of the agents that wait for the person. With a round running it lists the waiting rows as
// tickets, oldest first, the round's current stop marked with its place; with none, it counts them by glyph and
// says how to start. With nothing waiting and no round it draws nothing.

/** The fields of a row (as the agents kit hands it) this page reads. */
export type PassRow = {
  id: string;
  key: string;
  agent: string;
  project: { key: string; name: string };
  window: { instance: string; name: string };
  number: number;
  label: string;
  state: string;
  class: string;
  since: number;
  needs: { cards: number; questions: number; trust: boolean };
  waiting_since: number | null;
};
export type PassRound = {
  current: string | null;
  visited: string[];
  started_at: number;
  at: number;
  from: { instance: string; module: string; tab: number | null };
};
/** The part of the agents kit's row store this page reads. */
export type PassStore = {
  rows(): PassRow[];
  byKey(key: string): PassRow | undefined;
  selection(): string | null;
  round(): PassRound | null;
  on(fn: () => void): () => void;
};
export type Ticket = { row: PassRow; current: boolean; visited: boolean; selected: boolean };

const waited = (row: PassRow) => row.waiting_since ?? row.since;

/** The rows that wait for the person, the oldest wait first; equal waits keep the view's order. */
function waiting(rows: PassRow[]): PassRow[] {
  return rows.filter((row) => row.class === "needs" && row.key !== "").sort((a, b) => waited(a) - waited(b));
}

/** The round's stops as the host counts them: each waiting row, except that a window's trust rows, which one
 *  answer settles, are one stop. Each stop is the keys of its rows. */
function stops(rows: PassRow[]): string[][] {
  const out: string[][] = [];
  const trustOf = new Map<string, number>();
  for (const row of waiting(rows)) {
    if (row.needs.trust) {
      const at = trustOf.get(row.window.instance);
      if (at !== undefined) {
        out[at].push(row.key);
        continue;
      }
      trustOf.set(row.window.instance, out.length);
    }
    out.push([row.key]);
  }
  return out;
}

export function tickets(rows: PassRow[], round: PassRound | null, selection: string | null): Ticket[] {
  const visited = new Set(round?.visited ?? []);
  // A window's other trust rows belong to the current stop: they are where the round is, not behind it.
  const here = new Set(stops(rows).find((stop) => round?.current != null && stop.includes(round.current)) ?? []);
  return waiting(rows).map((row) => ({
    row,
    current: round?.current === row.key,
    visited: visited.has(row.key) && !here.has(row.key),
    selected: selection === row.key,
  }));
}

/** Whether a round runs: one at a stop that names a row still listed. A round that ended with the person in it is
 *  kept with no current stop, only so prefix B can go back to where it started; prefix u then starts a fresh one. A
 *  stop whose window went is no stop either: `n` goes on from it. */
export function running(round: PassRound | null, rows: PassRow[]): round is PassRound & { current: string } {
  return round !== null && round.current !== null && rows.some((row) => row.key === round.current);
}

/** Notes which waiting rows make up one stop (a window's trust rows), each key to its stop's first-seen key, so a
 *  stop answered at once still counts as one after its rows stopped waiting. */
export function noteStops(rows: PassRow[], groups: Map<string, string>): void {
  for (const stop of stops(rows)) {
    if (stop.length < 2) continue;
    const id = stop.map((key) => groups.get(key)).find((g) => g !== undefined) ?? stop[0];
    for (const key of stop) groups.set(key, id);
  }
}

/** Where the round stands: its current stop's place among the stops it has visited and those still to come.
 *  `groups` (from [noteStops]) keeps a stop whose rows no longer wait counted once. */
export function place(
  rows: PassRow[],
  round: PassRound | null,
  groups: Map<string, string> = new Map(),
): { at: number; of: number } | null {
  if (!round) return null;
  const all = stops(rows);
  const visited = new Set(round.visited);
  const here = round.current === null ? -1 : all.findIndex((stop) => stop.includes(round.current!));
  const done = new Set<number | string>();
  for (const key of round.visited) {
    if (key === round.current) continue;
    const at = all.findIndex((stop) => stop.includes(key));
    if (at === here && at >= 0) continue;
    done.add(at >= 0 ? at : `left:${groups.get(key) ?? key}`);
  }
  const ahead = all.filter((stop, at) => at !== here && !stop.some((key) => visited.has(key))).length;
  const now = round.current === null ? 0 : 1;
  return { at: done.size + now, of: done.size + now + ahead };
}

/** Whether the strip has anything to say: something waits, or a round runs. */
export function wanted(rows: PassRow[], round: PassRound | null): boolean {
  return running(round, rows) || rows.some((row) => row.class === "needs");
}

/** An age as `eitri agents` prints it: `41s`, `3m`, `1m35`, `1h12`. */
export function age(now: number, since: number): string {
  const seconds = Math.floor(Math.max(0, now - since) / 1000);
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) {
    const m = Math.floor(seconds / 60);
    const s = seconds % 60;
    return s === 0 ? `${m}m` : `${m}m${String(s).padStart(2, "0")}`;
  }
  return `${Math.floor(seconds / 3600)}h${String(Math.floor(seconds / 60) % 60).padStart(2, "0")}`;
}

// Every piece of text goes in as a text node: labels come from the person's prompts.
function span(cls: string, text: string): HTMLSpanElement {
  const el = document.createElement("span");
  el.className = cls;
  el.textContent = text;
  return el;
}

function ticketNode(t: Ticket, glyph: (row: PassRow) => string, now: number): HTMLElement {
  const el = document.createElement("span");
  el.className = "ticket";
  if (t.current) el.classList.add("current");
  if (t.visited) el.classList.add("visited");
  if (t.selected) el.classList.add("selected");
  el.dataset.key = t.row.key;
  el.dataset.state = t.row.state;
  const parts: Node[] = [];
  if (t.current) parts.push(span("marker", "▸"));
  parts.push(span("glyph", glyph(t.row)));
  parts.push(span("where", `${t.row.project.name}·${t.row.number}`));
  if (t.row.label) parts.push(span("label", t.row.label));
  parts.push(span("age", age(now, waited(t.row))));
  parts.forEach((part, i) => {
    if (i) el.append(document.createTextNode(" "));
    el.append(part);
  });
  return el;
}

function countsNode(rows: PassRow[], glyph: (row: PassRow) => string): HTMLElement {
  const counts = new Map<string, number>();
  for (const row of waiting(rows)) counts.set(glyph(row), (counts.get(glyph(row)) ?? 0) + 1);
  // Cards first, then questions, then any other glyph a newer state brings, in the order it first waited.
  const rank = (g: string) => (g === "⚑" ? 0 : g === "?" ? 1 : 2);
  const el = span("counts", "");
  [...counts.entries()]
    .sort((a, b) => rank(a[0]) - rank(b[0]))
    .forEach(([g, n], i) => {
      if (i) el.append(document.createTextNode("  "));
      const count = span("count", `${g} ${n}`);
      count.dataset.glyph = g;
      el.append(count);
    });
  return el;
}

export type StripDeps = {
  store: PassStore;
  glyph: (row: PassRow) => string;
  now?: () => number;
  /** Runs `fn` every `ms` until the returned function is called; ages tick only while the strip is up. */
  every?: (ms: number, fn: () => void) => () => void;
};

export function render(
  root: HTMLElement,
  deps: Required<StripDeps>,
  groups: Map<string, string> = new Map(),
): Ticket[] {
  const rows = deps.store.rows();
  const round = deps.store.round();
  const list = tickets(rows, round, deps.store.selection());
  if (!wanted(rows, round)) {
    root.replaceChildren();
    root.hidden = true;
    return list;
  }
  const now = deps.now();
  const parts: Node[] = [];
  if (running(round, rows)) {
    const at = place(rows, round, groups)!;
    parts.push(span("lead", at.of ? `round ${at.at}/${at.of}` : "round"));
    const line = span("tickets", "");
    list.forEach((t, i) => {
      if (i) line.append(span("sep", "│"));
      line.append(ticketNode(t, deps.glyph, now));
    });
    parts.push(line);
    parts.push(span("hint", "n next · prefix B back"));
  } else {
    parts.push(countsNode(rows, deps.glyph));
    parts.push(span("hint", "waiting · prefix u to start"));
  }
  root.replaceChildren(...parts);
  root.hidden = false;
  root.querySelector<HTMLElement>(".current")?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  return list;
}

/** Draws the strip from the store now and at every change, and keeps the ages current while it is up. */
export function mountStrip(root: HTMLElement, deps: StripDeps) {
  const full: Required<StripDeps> = {
    now: () => Date.now(),
    every: (ms, fn) => {
      const id = setInterval(fn, ms);
      return () => clearInterval(id);
    },
    ...deps,
  };
  let order: string[] = [];
  let tick: (() => void) | undefined;
  // The stops of the round that runs now, kept while it runs: a new round starts with none.
  const groups = new Map<string, string>();
  let roundOf: number | null = null;
  const draw = () => {
    const round = deps.store.round();
    const now = running(round, deps.store.rows()) ? round.started_at : null;
    if (now !== roundOf) groups.clear();
    roundOf = now;
    if (now !== null) noteStops(deps.store.rows(), groups);
    order = render(root, full, groups).map((t) => t.row.key);
    if (!root.hidden && !tick) tick = full.every(1000, draw);
    else if (root.hidden && tick) {
      tick();
      tick = undefined;
    }
  };
  const off = deps.store.on(draw);
  draw();
  return {
    /** The tickets' keys in the strip's order, for `j` and `k`. */
    order: () => order,
    stop() {
      off();
      tick?.();
      tick = undefined;
    },
  };
}
