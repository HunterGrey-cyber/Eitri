import type { Kit, Row } from "./agents";
import type { Group } from "./slots";

export type Model = {
  groups: readonly Group[];
  kit: Pick<Kit, "store" | "glyph" | "word">;
  marks: ReadonlySet<string>;
  filter: string;
  now: number;
  narrow: boolean;
  showIdle: boolean;
};

const CLASSES = new Set(["needs", "working", "quiet", "ended"]);
const KEYS_LINE = "j/k  ⏎ go  c new  x stop  / dim  m mark";

/** Every value from a row reaches the page as a text node, never as markup. */
function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.className = cls;
  if (text) node.textContent = text;
  return node;
}

const pad = (n: number) => String(n).padStart(2, "0");
/** `41s`, `1m35`, `16m`, `1h12`, `3d`: short enough for a sidebar, precise where it changes fast. */
export function age(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 10) return `${m}m${pad(s % 60)}`;
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h${pad(m % 60)}`;
  return `${Math.floor(h / 24)}d`;
}

/** How long a row has been what it is; a row that waits counts from its oldest wait. */
export function since(row: Row): number {
  return row.class === "needs" && typeof row.waiting_since === "number" ? row.waiting_since : row.since;
}

/** What waits on the row: cards, questions and a trust prompt. */
export function waiting(row: Row): number {
  return (row.needs?.cards ?? 0) + (row.needs?.questions ?? 0) + (row.needs?.trust ? 1 : 0);
}

function backgroundWord(bg: Row["background"]): string {
  if (!bg) return "";
  if (bg.shells) return `${bg.shells} sh`;
  if (bg.monitors) return `${bg.monitors} mon`;
  if (bg.agents) return `⏵${bg.agents}`;
  if (bg.other) return `${bg.other} bg`;
  return bg.calls ? `${bg.calls} bg` : "";
}

/** The one word beside the glyph: the tool, what runs in the background, or the state's own word. */
export function detail(row: Row, word: string): string {
  switch (row.state) {
    case "card": return row.tool ?? "card";
    case "question": return "asks";
    case "trust": return "trust";
    case "running": return row.background?.agents ? `⏵${row.background.agents}` : row.tool ?? "";
    case "background": return backgroundWord(row.background);
    case "starting": return "starting";
    case "done": return "done";
    case "idle": return "";
    case "new": return "new";
    case "failed": return "failed";
    case "ended": return row.ended_how === "stopped" ? "stopped" : "ended";
    default: return word;
  }
}

function tag(row: Row, word: string): string {
  return [`${row.window.name}·${row.number}`, row.label, row.project.name, row.agent, word, row.tool ?? ""].join(" ");
}

export function matches(row: Row, word: string, filter: string): boolean {
  return !filter || tag(row, word).toLowerCase().includes(filter.toLowerCase());
}

function rowNode(row: Row, m: Model, manyWindows: boolean): HTMLLIElement {
  const word = m.kit.word(row);
  const round = m.kit.store.round();
  const li = el("li", "row");
  li.dataset.key = row.key;
  li.dataset.state = row.state;
  li.classList.add(`class-${CLASSES.has(row.class) ? row.class : "unknown"}`);
  const out = row.class === "needs";
  const current = round?.current === row.key;
  const marked = m.marks.has(row.key);
  const hushed = !m.showIdle && row.state === "idle";
  li.classList.toggle("out", out);
  li.classList.toggle("current", current);
  li.classList.toggle("marked", marked);
  li.classList.toggle("cursor", m.kit.store.selection() === row.key);
  li.classList.toggle("dim", !matches(row, word, m.filter));
  li.classList.toggle("unread", row.unread);
  li.classList.toggle("moving", row.moving);
  li.classList.toggle("hushed", hushed);
  const name = `${row.project.name} ${row.window.name}·${row.number} ${row.label || row.agent}: ${word}`;
  if (m.narrow) {
    li.title = name;
    const count = out && waiting(row) > 0 ? String(waiting(row)) : "";
    li.append(el("span", "glyph", m.kit.glyph(row)), el("span", "count", count), el("span", "edge", out ? "◀" : ""));
    return li;
  }
  const number = manyWindows ? `${row.window.name}·${row.number}` : String(row.number);
  const num = el("span", "num", number);
  if (manyWindows) num.title = number;
  const label = el("span", "label", hushed ? "" : row.label || row.agent);
  if (!row.label) label.classList.add("unnamed");
  li.append(
    el("span", "stop", current ? "▸" : marked ? "*" : ""),
    num,
    label,
    el("span", "glyph", m.kit.glyph(row)),
    el("span", "what", hushed ? "" : row.moving ? "moving" : detail(row, word)),
    el("span", "age", hushed || row.state === "new" ? "" : age(m.now - since(row))),
    el("span", "edge", out ? "◀" : ""),
  );
  return li;
}

/** Monospace cells a number takes: two for a wide (CJK, fullwidth) character, one for the rest. */
export function cells(text: string): number {
  let n = 0;
  for (const ch of text) n += /[\u1100-\u115f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe30-\ufe4f\uff00-\uff60\uffe0-\uffe6]/u.test(ch) ? 2 : 1;
  return n;
}

/** The widest number the column holds; past it a number ends in an ellipsis and its title says it whole. */
const NUM_MAX_CELLS = 16;

function emptyNode(narrow: boolean): HTMLLIElement {
  const li = el("li", "row empty");
  li.append(el("span", narrow ? "glyph" : "label", "┄"));
  if (!narrow) li.prepend(el("span", "stop"), el("span", "num"));
  return li;
}

/** One line in place of the list: the layer is off, or the page is not in Eitri. */
export function renderLine(root: HTMLElement, text: string): void {
  const frame = el("div", "strips");
  frame.append(el("p", "off", text));
  root.replaceChildren(frame);
}

export function render(root: HTMLElement, m: Model): void {
  const frame = el("div", m.narrow ? "strips narrow" : "strips");
  const list = el("ol", "list");
  let total = 0;
  let needs = 0;
  let numCells = 0;
  for (const group of m.groups) {
    const header = el("li", "project", m.narrow ? "" : group.name);
    if (m.narrow) header.title = group.name;
    list.append(header);
    const rows = group.slots.map(slot => slot.key === null ? undefined : m.kit.store.byKey(slot.key));
    const manyWindows = new Set(rows.flatMap(row => row ? [row.window.instance] : [])).size > 1;
    if (manyWindows && !m.narrow) {
      for (const row of rows) if (row) numCells = Math.max(numCells, cells(`${row.window.name}·${row.number}`));
    }
    for (const row of rows) {
      if (!row) {
        list.append(emptyNode(m.narrow));
        continue;
      }
      total += 1;
      if (row.class === "needs") needs += 1;
      list.append(rowNode(row, m, manyWindows));
    }
  }
  // Only a list that names windows widens the column; a bare tab number fits the stylesheet's default.
  if (numCells) list.style.setProperty("--num-w", `${Math.min(numCells, NUM_MAX_CELLS)}ch`);
  if (!m.narrow) {
    const head = el("header", "head");
    const sum = needs ? `${needs} waiting` : total ? String(total) : "";
    head.append(el("span", "title", "agents"), el("span", "sum", sum));
    frame.append(head);
  }
  if (!m.groups.length) frame.append(el("p", "none", m.narrow ? "·" : "no agents"));
  frame.append(list);
  if (!m.narrow) frame.append(el("footer", "keys", KEYS_LINE));
  root.replaceChildren(frame);
}
