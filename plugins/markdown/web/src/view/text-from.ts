import { charStart } from "./chars";
import { SKIP } from "./rows";

/** A vertical extent in document coordinates. */
export interface Span { top: number; bottom: number }

/** How many children an element may have for each of them to be read; past this many, laid out one after another,
 *  the first that reaches the band is found by halving. */
const FEW = 8;

/** What `eachTextAt` reads: `skip` is the elements whose text it never visits, `blanks` whether a text node of white
 *  space alone is visited. In code one that holds a space or a tab always is: those are drawn there. */
export interface TextAtOptions { skip: string; blanks: boolean }

/** How long a text node may be for its extent to be read with one range over all of it: so short a node spans a few
 *  lines at most, and a highlighted code token or a word of prose costs one read. */
const SHORT = 32;

/** A text node's vertical extent (document coordinates): the top of its first line box and the bottom of its last,
 *  or null for a node with no box. A node longer than `SHORT` has each end read from a short span of its text at that
 *  end (one character, then two, four... until one has a box), so a node spanning many lines (an unhighlighted code
 *  block, a long comment token) costs a few short ranges and never a range over all its lines. */
export function textSpan(t: Text, range: Range = t.ownerDocument.createRange()): Span | null {
  const n = t.data.length;
  const y = window.scrollY;
  // The boxes of the span [`a`, `b`) together, or null when it has none.
  const read = (a: number, b: number): Span | null => {
    range.setStart(t, a);
    range.setEnd(t, b);
    const rects = Array.from(range.getClientRects?.() ?? []);
    if (rects.length === 0) return null;
    return { top: Math.min(...rects.map((r) => r.top)) + y, bottom: Math.max(...rects.map((r) => r.bottom)) + y };
  };
  const edge = (end: boolean): Span | null => {
    for (let k = 1; ; k = Math.min(n, 2 * k)) {
      const at = end ? read(n - k, n) : read(0, k);
      if (at !== null || k >= n) return at;
    }
  };
  if (n === 0) return null;
  if (n <= SHORT) return read(0, n);
  const first = edge(false);
  const last = first && edge(true);
  return first && last ? { top: first.top, bottom: last.bottom } : null;
}

/** Whether `t`, white space alone, is drawn: it holds a space or a tab and is code, where those keep their width. */
function drawnBlank(t: Text): boolean { return /[ \t]/.test(t.data) && t.parentElement?.closest("pre") != null; }

/** A cell spanning rows: the row of its section it starts on and how many rows it covers. */
interface Spanning { row: number; rows: number }

/** The cells of a table section that span rows (raw HTML's `rowspan`; `0` covers the rest of the section, and a span
 *  is cut at the section's end, as it is drawn): none for a section where no cell does (every Markdown table), and for
 *  anything not a section. */
function spanningCells(el: Element): Spanning[] {
  if (!(el instanceof HTMLTableSectionElement)) return [];
  const n = el.rows.length;
  const out: Spanning[] = [];
  for (const cell of el.querySelectorAll<HTMLTableCellElement>(":scope > tr > [rowspan]")) {
    const row = (cell.parentElement as HTMLTableRowElement).sectionRowIndex;
    const rows = Math.min(n - row, cell.rowSpan === 0 ? n : cell.rowSpan);
    if (rows > 1) out.push({ row, rows });
  }
  return out;
}

/**
 * Calls `visit` with each text node under `scope` whose box reaches below `band.top` and starts above `band.bottom`
 * (document coordinates; an infinite bottom takes in everything below), in document order, until `visit` returns
 * true; returns whether it did.
 *
 * The walk goes down through elements by their own boxes (`getBoundingClientRect`), so an element whose box does not
 * meet the band, or that has none (a closed `details`' folded content, a hidden element), is passed over whole
 * whatever it holds and wherever it comes in the document: the cells of a table row are each looked into, and a cell's
 * text below the band is never taken for the row's. Among many children laid out one after another (anything but a
 * flex or grid container) the first that reaches the band is found by halving and the walk stops at the first that
 * starts below it, so a highlighted code block of thousands of nodes costs a few reads and the row's own nodes. A
 * text node's box is its own line boxes, its two ends read by `textSpan`, only for a node on the way.
 *
 * A table section in which a cell spans rows (raw HTML's `rowspan`) is the exception: such a cell is drawn down past
 * its own row's box, so the rows from the first row of a spanning cell that covers the band's first row are looked
 * into, and their cells that meet the band are visited left to right (the spanning cell comes in the document before
 * the cells of the later rows it sits beside, wherever it is drawn); with an infinite bottom, row by row, each row's
 * extent read as the band (so a spanning cell's text is visited only where it is drawn) and left to right in it, so
 * the nearest row below comes first and no row after it is read.
 */
export function eachTextAt(scope: Element, band: Span, opts: TextAtOptions, visit: (t: Text) => boolean): boolean {
  const range = scope.ownerDocument.createRange();
  const y = window.scrollY;
  const box = (el: Element): DOMRect | null => {
    if (el.matches(opts.skip)) return null;
    const r = el.getBoundingClientRect();
    return r.width === 0 && r.height === 0 ? null : r;
  };
  const extent = (n: Node): Span | null => {
    if (n.nodeType === Node.TEXT_NODE) {
      const t = n as Text;
      if (t.data === "" || (!opts.blanks && !/\S/.test(t.data) && !drawnBlank(t))) return null;
      return textSpan(t, range);
    }
    const r = n.nodeType === Node.ELEMENT_NODE ? box(n as Element) : null;
    return r ? { top: r.top + y, bottom: r.bottom + y } : null;
  };
  // The cells of `section` that meet `b`, left to right: those of row `first` (the first row reaching `b`) and the
  // rows after it that start above `b`'s bottom, and from an earlier row only a cell spanning down to `first`.
  const cellsAt = (section: HTMLTableSectionElement, spanning: Spanning[], first: number, b: Span): Element[] => {
    const rows = section.rows;
    let from = first;
    for (const s of spanning) if (s.row < from && s.row + s.rows > first) from = s.row;
    const cells: { cell: Element; left: number }[] = [];
    for (let i = from; i < rows.length; i++) {
      const e = extent(rows[i]);
      if (i >= first && e && e.top >= b.bottom) break;
      for (const cell of Array.from(rows[i].cells)) {
        if (i < first && cell.rowSpan === 1) continue;
        const r = box(cell);
        if (r && r.bottom + y > b.top && r.top + y < b.bottom) cells.push({ cell, left: r.left });
      }
    }
    return cells.sort((a, c) => a.left - c.left).map((c) => c.cell);
  };
  // With no bottom, row by row from the first reaching `b`, each row's own extent read as a band of its own: a cell
  // spanning down past its row is looked into only where it meets the row being read, so its text drawn further
  // down (vertically centred in a later row) never comes before a nearer row's, and the walk stops at the first row
  // that has text.
  const across = (section: HTMLTableSectionElement, spanning: Spanning[], b: Span): boolean => {
    const rows = section.rows;
    const first = firstReaching(rows, extent, b.top);
    if (b.bottom !== Infinity) return cellsAt(section, spanning, first, b).some((cell) => down(cell, b));
    for (let i = first; i < rows.length; i++) {
      const e = extent(rows[i]);
      if (!e) continue;
      const row = { top: Math.max(b.top, e.top), bottom: e.bottom };
      if (cellsAt(section, spanning, i, row).some((cell) => down(cell, row))) return true;
    }
    return false;
  };
  const down = (el: Element, b: Span): boolean => {
    const spanning = spanningCells(el);
    if (spanning.length > 0) return across(el as HTMLTableSectionElement, spanning, b);
    const kids = el.childNodes;
    const ordered = kids.length > FEW && !/flex|grid/.test(getComputedStyle(el).display);
    for (let k = ordered ? firstReaching(kids, extent, b.top) : 0; k < kids.length; k++) {
      const e = extent(kids[k]);
      if (!e || e.bottom <= b.top) continue;
      if (e.top >= b.bottom) {
        if (ordered) break;
        continue;
      }
      const kid = kids[k];
      if (kid.nodeType === Node.TEXT_NODE ? visit(kid as Text) : down(kid as Element, b)) return true;
    }
    return false;
  };
  const found = down(scope, band);
  range.detach?.();
  return found;
}

/* The first of `kids` whose box reaches below `top`, by halving; a child with no box reads as the first one after it
   that has one, and one past the last as reaching. */
function firstReaching(kids: ArrayLike<Node>, extent: (n: Node) => Span | null, top: number): number {
  const reachesFrom = (i: number, end: number) => {
    for (let k = i; k < end; k++) { const e = extent(kids[k]); if (e) return e.bottom > top; }
    return true;
  };
  let first = 0;
  for (let end = kids.length; first < end;) {
    const m = (first + end) >> 1;
    if (reachesFrom(m, end)) end = m;
    else first = m + 1;
  }
  return first;
}

/** The first character, white space passed over, of the row at band `b` (document coordinates) in `scope`'s text, as
 *  a collapsed range: the first in document order whose middle is on the band. A code line of spaces alone (drawn,
 *  as all white space in code but a line break is) gives its first space. A row with no such character (an image, an
 *  empty code line) gives the first one below it. Null when there is none. */
export function textFrom(scope: Element, b: Span): Range | null {
  const doc = scope.ownerDocument;
  const range = doc.createRange();
  const y = window.scrollY;
  const mid = (node: Text, i: number) => {
    range.setStart(node, i);
    range.setEnd(node, i + 1);
    const r = range.getBoundingClientRect();
    return (r.top + r.bottom) / 2 + y;
  };
  const onBand = (node: Text, i: number) => { const m = mid(node, i); return m >= b.top - 0.5 && m <= b.bottom + 0.5; };
  const at = (node: Text, i: number) => { const r = doc.createRange(); r.setStart(node, i); return r; };
  // The first space or tab of the band's row in code, in case the row holds nothing else.
  let blank: Range | null = null;
  // The first character of `text` whose middle is at or below the band's top that is not white space; with `onRow`,
  // null when that character is not on the band (noting a drawn space of the band's row in code as `blank`).
  const firstChar = (text: Text, onRow: boolean): Range | null => {
    const data = text.data;
    let lo = 0;
    let hi = data.length - 1;
    while (lo < hi) {
      const m = (lo + hi) >> 1;
      if (mid(text, m) >= b.top - 0.5) hi = m;
      else lo = m + 1;
    }
    lo = charStart(text, lo);
    let k = lo;
    while (k < data.length && /\s/.test(data[k])) k++;
    if (k < data.length && (!onRow || onBand(text, k))) return at(text, k);
    if (onRow && blank === null && drawnBlank(text)) {
      let s = lo;
      while (s < data.length && data[s] === "\n") s++;
      if ((data[s] === " " || data[s] === "\t") && onBand(text, s)) blank = at(text, s);
    }
    return null;
  };
  let found: Range | null = null;
  const opts = { skip: SKIP, blanks: false };
  eachTextAt(scope, b, opts, (t) => (found = firstChar(t, true)) !== null);
  found ??= blank;
  if (!found) eachTextAt(scope, { top: b.top, bottom: Infinity }, opts, (t) => (found = firstChar(t, false)) !== null);
  range.detach?.();
  return found;
}
