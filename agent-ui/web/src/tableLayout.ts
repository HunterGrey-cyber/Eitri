/* A markdown table in a reply is a grid while the grid reads well, and a list once it does not.

   The grid is the browser's own table layout: each column gets its longest word and a share of what is left. That
   fails when the text has no short words to give up (a column of long paths) or when the box is narrow: a column ends
   up two characters wide, a cell ten lines tall, or the table scrolls sideways. The rule, measured after layout:

   - the grid stays while the table fits its box and no cell takes more than `MAX_CELL_LINES` lines;
   - otherwise every row is drawn as a block of `Header: value` lines, one per cell, which wraps at any width.

   The decision is made once, when the table is whole (never for a table still being written, so a row arriving cannot
   flip it) and again when the panel's width changes. The list is its own markup, built from the table's cells with DOM
   calls (the cells were sanitized when the reply was drawn, and nothing here parses text), and the grid stays beside it
   with the `hidden` attribute, so what is read, searched and selected is the one that is shown. */

/** A cell taking more lines than this sends its table to the list. */
export const MAX_CELL_LINES = 4;
/** A width change smaller than this is not worth a second look: a scrollbar appearing because the list made the page
 *  taller changes the width by about this much, and deciding again on it could flip the table back and forth. */
export const RESIZE_SLACK = 24;

export type TableMeasure = {
  /** The width of the box the table sits in. */
  boxWidth: number;
  /** The width the grid takes, which is more than the box's when a column's smallest width does not fit. */
  tableWidth: number;
  /** The lines each cell's content takes in the grid. */
  cellLines: number[];
};

/** Whether the grid reads well (see the top of the file). A pixel of rounding in the widths does not count. */
export function chooseLayout(measure: TableMeasure): "grid" | "list" {
  if (measure.tableWidth > measure.boxWidth + 1) return "list";
  return measure.cellLines.some((lines) => lines > MAX_CELL_LINES) ? "list" : "grid";
}

type Box = { top: number; bottom: number; height: number; width: number };

/** The lines the boxes of one cell's content fall on. A box starts a new line when its middle is below everything on
 *  the line before, so a code span a few pixels taller than the text around it stays on the text's line. Boxes with no
 *  width (a break opportunity at a line's end) are not content. */
export function countLines(boxes: Iterable<Box>): number {
  const sorted = Array.from(boxes).filter((b) => b.width > 0 && b.height > 0).sort((a, b) => a.top - b.top);
  let lines = 0;
  let lineBottom = -Infinity;
  for (const box of sorted) {
    if ((box.top + box.bottom) / 2 > lineBottom) lines += 1;
    lineBottom = Math.max(lineBottom, box.bottom);
  }
  return lines;
}

function tableOf(wrapper: Element): HTMLTableElement | null {
  return Array.from(wrapper.children).find((el): el is HTMLTableElement => el instanceof HTMLTableElement) ?? null;
}

/** The row of column headers: the first row of the `thead`, or a first row made of `th` cells. */
function headRowOf(table: HTMLTableElement): HTMLTableRowElement | undefined {
  return table.tHead?.rows[0] ?? (table.rows[0]?.cells[0]?.tagName === "TH" ? table.rows[0] : undefined);
}

/** The elements markdown writes in a cell, and the inline ones a reply's HTML may add. */
const INLINE = new Set([
  "A", "ABBR", "B", "BR", "CITE", "CODE", "DEL", "DFN", "EM", "I", "INS", "KBD", "MARK", "Q", "S", "SAMP", "SMALL", "SPAN",
  "STRONG", "SUB", "SUP", "TIME", "TT", "U", "VAR", "WBR",
]);

/** Whether the list can stand in for the table: every cell holds only inline content, as markdown writes them, and the
 *  table has no caption and one header section. A reply's own HTML can put blocks, a fence, a nested table, a caption
 *  or a second header in a table; those stay a grid, because a copy in a list would show a fence or a drawn diagram
 *  twice, and a second header's rows would be listed as data. */
function listable(table: HTMLTableElement): boolean {
  if (table.caption !== null || table.querySelectorAll(":scope > thead").length > 1) return false;
  return Array.from(table.querySelectorAll("td *, th *")).every((el) => INLINE.has(el.tagName));
}

/** The list form of `table`: a `div.table-list` of one `ul.table-list-row` per body row, each `li.table-list-cell`
 *  holding `<strong class="table-list-label">Header:</strong>` and a copy of the cell's content. An empty cell is left
 *  out, and a column whose header has no text gets no label. The table is not changed. */
export function buildTableList(table: HTMLTableElement): HTMLElement {
  const head = headRowOf(table);
  const list = document.createElement("div");
  list.className = "table-list";
  for (const row of Array.from(table.rows)) {
    if (row === head) continue;
    const item = document.createElement("ul");
    item.className = "table-list-row";
    Array.from(row.cells).forEach((cell, column) => {
      if ((cell.textContent ?? "").trim() === "" && cell.children.length === 0) return;
      const entry = document.createElement("li");
      entry.className = "table-list-cell";
      const header = head?.cells[column];
      if (header !== undefined && (header.textContent ?? "").trim() !== "") {
        const label = document.createElement("strong");
        label.className = "table-list-label";
        for (const child of Array.from(header.childNodes)) label.append(child.cloneNode(true));
        label.append(":");
        entry.append(label, " ");
      }
      for (const child of Array.from(cell.childNodes)) entry.append(child.cloneNode(true));
      item.append(entry);
    });
    if (item.childNodes.length > 0) list.append(item);
  }
  return list;
}

type State = {
  /** The box width the table was last decided at; 0 until it has been laid out. */
  width: number;
  /** The table's font size then: a change of text size changes what fits without changing the width. */
  fontSize: string;
  list: HTMLElement | null;
};

const states = new WeakMap<Element, State>();
const observers = new WeakMap<Element, ResizeObserver>();

/** The text of `cell` as lines: one box per line of each piece of text. Not `selectNodeContents(cell)`: that also
 *  returns the border box of an inline-block (inline code), which wraps over several lines and is one tall box. */
function lineBoxes(cell: Element, range: Range): Box[] {
  const boxes: Box[] = [];
  const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    range.selectNodeContents(walker.currentNode);
    boxes.push(...Array.from(range.getClientRects()));
  }
  return boxes;
}

/** What the engine laid out, read now. `null` where nothing is laid out (jsdom). */
function measure(wrapper: HTMLElement, table: HTMLTableElement): TableMeasure | null {
  if (typeof Range.prototype.getClientRects !== "function") return null;
  const range = document.createRange();
  const cellLines: number[] = [];
  for (const row of Array.from(table.rows)) {
    for (const cell of Array.from(row.cells)) cellLines.push(countLines(lineBoxes(cell, range)));
  }
  return { boxWidth: wrapper.clientWidth, tableWidth: table.offsetWidth, cellLines };
}

type Pending = { wrapper: HTMLElement; table: HTMLTableElement; state: State };

/** Measures the grid of each of `pending` and shows the form `chooseLayout` picks. All the grids are shown, then all
 *  measured, then all switched, so the engine lays the page out once for the reading however many tables there are
 *  (a write between two readings makes the next one lay the whole list out again). This is one call, so no frame is
 *  drawn with the grids shown. A box with no width (not laid out: a hidden tab) is left alone. */
function decideAll(pending: Pending[]): void {
  const laidOut = pending.filter(({ wrapper, table }) => wrapper.clientWidth !== 0 && listable(table));
  for (const { table, state } of laidOut) {
    table.hidden = false;
    if (state.list !== null) state.list.hidden = true;
  }
  const measured = laidOut.map((p) => ({ p, m: measure(p.wrapper, p.table), fontSize: getComputedStyle(p.table).fontSize }));
  for (const { p, m, fontSize } of measured) {
    if (m === null) continue;
    p.state.width = m.boxWidth;
    p.state.fontSize = fontSize;
    if (chooseLayout(m) === "grid") continue;
    p.state.list ??= p.wrapper.appendChild(buildTableList(p.table));
    p.table.hidden = true;
    p.state.list.hidden = false;
  }
}

/** Whether the page's selection has an end inside `wrapper`: the reader's CARET, VISUAL or V-LINE selection. Its end
 *  is not moved out from under it by showing the other form. */
function holdsSelection(wrapper: Element): boolean {
  const selection = getSelection();
  return selection !== null && [selection.anchorNode, selection.focusNode].some((node) => node !== null && wrapper.contains(node));
}

/** Tables left as they were because the selection had an end in them, to be decided when the selection moves. */
const guarded = new Map<HTMLElement, Pending>();

function decideGuarded(): void {
  const ready = [...guarded.values()].filter((p) => !p.wrapper.isConnected || !holdsSelection(p.wrapper));
  for (const p of ready) guarded.delete(p.wrapper);
  decideAll(ready.filter((p) => p.wrapper.isConnected && states.get(p.wrapper) === p.state));
  if (guarded.size === 0) document.removeEventListener("selectionchange", decideGuarded);
}

function guard(p: Pending): void {
  if (guarded.size === 0) document.addEventListener("selectionchange", decideGuarded);
  guarded.set(p.wrapper, p);
}

function watch(root: HTMLElement): ResizeObserver | null {
  let observer = observers.get(root);
  if (observer === undefined) {
    if (typeof ResizeObserver === "undefined") return null;
    observer = new ResizeObserver((entries) => {
      const again: Pending[] = [];
      for (const { target } of entries) {
        const state = states.get(target);
        const table = tableOf(target);
        if (!target.isConnected || state === undefined || table === null) {
          observer!.unobserve(target);
          continue;
        }
        const moved = Math.abs((target as HTMLElement).clientWidth - state.width) >= RESIZE_SLACK;
        const resized = state.width === 0 || moved || getComputedStyle(table).fontSize !== state.fontSize;
        if (!resized) continue;
        const pending = { wrapper: target as HTMLElement, table, state };
        if (holdsSelection(target)) guard(pending);
        else again.push(pending);
      }
      decideAll(again);
    });
    observers.set(root, observer);
  }
  return observer;
}

/** Lays out the tables under `root` that are not laid out yet. `settledUpTo` is how far the reply is complete: only
 *  the children of `root` up to and including it are looked at (`null`: none), because a table in the block still
 *  being written is not whole. Omitted: all of them. */
export function layoutTables(root: HTMLElement, settledUpTo?: ChildNode | null): void {
  if (settledUpTo === null) return;
  const fresh: Pending[] = [];
  for (let node = root.firstChild; node !== null; node = node.nextSibling) {
    if (node instanceof Element) {
      const wrappers = node.matches(".table-scroll") ? [node] : Array.from(node.querySelectorAll(".table-scroll"));
      for (const wrapper of wrappers) {
        const table = tableOf(wrapper);
        // A wrapper inside a table is a reply's own nested table, part of its cell.
        if (states.has(wrapper) || table === null || wrapper.parentElement?.closest("table")) continue;
        const state: State = { width: 0, fontSize: "", list: null };
        states.set(wrapper, state);
        fresh.push({ wrapper: wrapper as HTMLElement, table, state });
      }
    }
    if (node === settledUpTo) break;
  }
  decideAll(fresh);
  const observer = fresh.length > 0 ? watch(root) : null;
  for (const { wrapper } of fresh) observer?.observe(wrapper);
}

/** Stops watching `root`'s tables and puts each back to the grid it was drawn as (the box is going, or its content is
 *  drawn again: running `layoutTables` over the same content afterwards starts from the same place). */
export function forgetTables(root: HTMLElement): void {
  observers.get(root)?.disconnect();
  observers.delete(root);
  for (const wrapper of root.querySelectorAll(".table-scroll")) {
    const state = states.get(wrapper);
    if (state === undefined) continue;
    guarded.delete(wrapper as HTMLElement);
    state.list?.remove();
    const table = tableOf(wrapper);
    if (table !== null) table.hidden = false;
    states.delete(wrapper);
  }
}
