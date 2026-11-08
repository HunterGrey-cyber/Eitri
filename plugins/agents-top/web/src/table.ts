import type { Row, Store } from "../../../../plugin-kit/agents/dist/store";
import { SORTS, sortRows } from "./sort";
import type { Sort } from "./sort";

export type Kit = { store: Store; glyph(row: Row): string; word(row: Row): string };
export function age(since: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m${String(seconds % 60).padStart(2, "0")}`;
  return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}`;
}
/** How long until [age] of `since` reads differently at `now`: a second while it shows seconds (under an hour), a
 *  minute after that. */
export function ageChangesIn(since: number, now = Date.now()): number {
  const elapsed = Math.max(0, now - since);
  const step = elapsed < 3_600_000 ? 1_000 : 60_000;
  return (Math.floor(elapsed / step) + 1) * step - (now - since);
}
// A text or attribute set to the value it already has still counts as a change to the page, which then styles, lays
// out and paints again; these write only what differs.
function setText(el: Node, value: string): void {
  if (el.textContent !== value) el.textContent = value;
}
function setAttr(el: Element, name: string, value: string): void {
  if (el.getAttribute(name) !== value) el.setAttribute(name, value);
}
function background(row: Row): string {
  const bg = row.background;
  if (!bg.calls) return "";
  const kinds = [[bg.shells, "sh"], [bg.monitors, "mon"], [bg.agents, "agents"], [bg.other, "other"]]
    .filter(([count]) => Number(count) > 0).map(([count, name]) => `${count} ${name}`);
  return `${bg.calls}${kinds.length ? ` (${kinds.join(", ")})` : ""}`;
}
/** Scrolls the page only as far as it takes to show `el` whole, top to bottom, and never sideways: a row is as wide as
 *  the table, so a scroll that also lined it up horizontally would move the page off its first column. */
export function reveal(el: Element, view: Window = window): void {
  const box = el.getBoundingClientRect();
  const height = view.innerHeight;
  if (box.top < 0) view.scrollBy(0, box.top);
  else if (box.bottom > height) view.scrollBy(0, Math.min(box.bottom - height, box.top));
}
export function createTable(root: HTMLElement, kit: Kit, select?: (key: string) => void) {
  let sort: Sort = "state";
  let reverse = false;
  let query = "";
  let visible: Row[] = [];
  // Rows keep their element across redraws, so a click that began on one still lands when the clock redraws mid-click.
  const rowEls = new Map<string, HTMLTableRowElement>();
  const columns = ["state", "age", "project", "window", "number", "agent", "mode", "tool", "background", "label"];
  let scrolledTo: string | null = null;
  const heading = document.createElement("h1");
  const controls = document.createElement("div");
  const sortButton = document.createElement("button");
  const reverseButton = document.createElement("button");
  reverseButton.textContent = "Reverse (S)";
  controls.append(sortButton, reverseButton);
  const table = document.createElement("table");
  const head = table.createTHead().insertRow();
  ["STATE", "AGE", "PROJECT", "WIN", "#", "AGENT", "MODE", "TOOL", "BG", "LABEL"].forEach((name, i) => {
    const cell = document.createElement("th");
    cell.scope = "col";
    cell.textContent = name;
    // The stylesheet drops a column from the header and the rows together, by name, when the module is narrow.
    cell.dataset.column = columns[i];
    head.append(cell);
  });
  const body = table.createTBody();
  const empty = document.createElement("p");
  const footer = document.createElement("footer");
  footer.textContent = "j/k move · Enter go/round · c new · x stop · s sort · S reverse · / filter · Esc editor · host keys close";
  root.replaceChildren(heading, controls, table, empty, footer);
  // One timer, for the moment the earliest age on screen reads differently; none while no row shows one, or while
  // the page is not on screen (it draws again when it comes back).
  let clock: number | undefined;
  let clockAt: number | null = null;
  function schedule(next: number | null) {
    const want = document.hidden ? null : next;
    if (want === clockAt && (want === null || clock !== undefined)) return;
    if (clock !== undefined) window.clearTimeout(clock);
    clock = undefined;
    clockAt = want;
    if (want === null) return;
    clock = window.setTimeout(() => {
      clock = undefined;
      clockAt = null;
      draw();
    }, Math.max(0, want - Date.now()));
  }
  function draw() {
    const rows = kit.store.rows();
    const now = Date.now();
    let next: number | null = null;
    setText(heading, `eitri agents — ${rows.length} agents, ${rows.filter(r => r.class === "needs").length} waiting`);
    setText(sortButton, `Sort: ${sort} ${reverse ? "▴" : "▾"} (s)`);
    setAttr(reverseButton, "aria-pressed", String(reverse));
    visible = sortRows(rows, sort, reverse).filter(row => [kit.word(row), row.project.name, row.window.name,
      String(row.number), row.agent, row.mode, row.tool ?? "", row.label].join(" ").toLowerCase().includes(query));
    const round = kit.store.round();
    const visited = new Set(round?.visited ?? []);
    const selected = kit.store.selection();
    const seen = new Set<string>();
    for (const row of visible) {
      seen.add(row.key);
      let tr = rowEls.get(row.key);
      if (!tr) {
        tr = document.createElement("tr");
        tr.dataset.key = row.key;
        for (const column of columns) {
          const cell = tr.insertCell();
          cell.dataset.column = column;
          // The value sits in its own box, which the stylesheet can cut short (a table cell takes no max-width).
          cell.append(document.createElement("span"));
        }
        if (select) {
          const key = row.key;
          tr.addEventListener("click", () => select(key));
        }
        rowEls.set(row.key, tr);
      }
      tr.classList.toggle("selected", selected === row.key);
      tr.classList.toggle("current", round?.current === row.key);
      tr.classList.toggle("visited", visited.has(row.key));
      setAttr(tr, "aria-label", `${row.label}${visited.has(row.key) ? ", visited" : ""}${round?.current === row.key ? ", current round stop" : ""}`);
      const marker = round?.current === row.key ? "▸" : visited.has(row.key) ? "✓" : " ";
      const at = now + ageChangesIn(row.since, now);
      if (next === null || at < next) next = at;
      const values = [`${marker}${kit.glyph(row)} ${kit.word(row)}`, age(row.since, now), row.project.name,
        row.window.name, String(row.number), row.agent, row.mode, row.tool ?? "", background(row), row.label];
      values.forEach((value, i) => {
        const cell = tr.cells[i];
        const box = cell.firstElementChild as HTMLElement;
        if (box.textContent !== value) box.textContent = value;
        // A value the stylesheet cuts short says itself whole on hover.
        if (cell.title !== value) cell.title = value;
      });
    }
    for (const key of [...rowEls.keys()]) if (!seen.has(key)) rowEls.delete(key);
    const ordered = visible.map(row => rowEls.get(row.key)!);
    if (ordered.length !== body.rows.length || ordered.some((tr, i) => body.rows[i] !== tr)) body.replaceChildren(...ordered);
    if (selected !== scrolledTo) {
      scrolledTo = selected;
      const tr = selected ? rowEls.get(selected) : undefined;
      if (tr) reveal(tr);
    }
    setText(empty, visible.length ? "" : rows.length ? "No matching agents" : "No window shares agents");
    schedule(next);
  }
  const onVisibility = () => { if (document.hidden) schedule(null); else draw(); };
  function handleKey(key: string): boolean {
    if (key === "s") sort = SORTS[(SORTS.indexOf(sort) + 1) % SORTS.length];
    else if (key === "S") reverse = !reverse;
    else return false;
    draw();
    return true;
  }
  sortButton.onclick = () => handleKey("s");
  reverseButton.onclick = () => handleKey("S");
  const remove = kit.store.on(draw);
  document.addEventListener("visibilitychange", onVisibility);
  draw();
  return {
    order: () => visible.map(row => row.key),
    sort: () => sort,
    setSort(value: Sort) { sort = value; draw(); },
    setFilter(value: string) { query = value.toLowerCase(); draw(); },
    handleKey,
    destroy() {
      remove();
      document.removeEventListener("visibilitychange", onVisibility);
      if (clock !== undefined) window.clearTimeout(clock);
      clock = undefined;
      root.replaceChildren();
    },
  };
}
