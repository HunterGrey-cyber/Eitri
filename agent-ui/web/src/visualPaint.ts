/**
 * VISUAL and V-LINE's selection, drawn by the panel instead of the engine's `::selection`.
 *
 * WebKit paints a selection with gaps: a table cell's padding up to the next selected cell and a
 * block's width past the end of its text. A selection through a table's row therefore showed the
 * cells as boxes of different widths and heights, with a sliver above the last one. Here the list
 * carries a layer of boxes instead (`.visual-paint`, a child of `.message-list` beside the cursor
 * band, in the list's own scrolling coordinates) and `index.css` makes the engine's selection
 * transparent in those two modes. CARET keeps the engine's one reversed character, which has no gap
 * to fill.
 *
 * VISUAL: the selected characters' boxes, gathered into rows, joined along a row over a space or a
 * code span's padding but never across table cells, cut to the sides of a block that scrolls
 * sideways, each row as tall as its line (the leading shared with the rows around it, so a wrapped
 * paragraph's rows meet). V-LINE: one band from the first selected row's top to the last one's
 * bottom (a table row's whole box at either end), across the list as the cursor band is.
 */

import { VISUAL_CHROME } from "./visual";

/** A box in the list's content coordinates (what `top`/`left` mean for its absolute children). */
export type PaintBox = { top: number; bottom: number; left: number; right: number };

/** Blocks that scroll sideways: what lies past their sides is clipped, and so is a box over it. */
const SCROLLERS = ".code-block, .diagram-scroll, .table-scroll, .permission-card-diff";
/** Elements whose own children are boxes, never text: white space directly inside them is not laid out. */
const STRUCTURE = "table, thead, tbody, tfoot, tr, ul, ol";
/** Text that is not the selection's: VISUAL's chrome (`visual.ts`, what `y` leaves out: row signs, buttons, fold
 *  markers, what is hidden from readers), and what is never drawn. */
const NOT_TEXT = [...VISUAL_CHROME, "[hidden]", "script", "style", "template"].join(", ");
/** How much of the shorter of two boxes must overlap the other, vertically, for both to be on one row. */
const SAME_ROW = 0.5;

type Part = PaintBox & { cell: Element | null; line: number };

function lineHeightOf(el: Element | null): number {
  if (!el) return 0;
  const style = getComputedStyle(el);
  const line = parseFloat(style.lineHeight);
  if (Number.isFinite(line)) return line;
  const size = parseFloat(style.fontSize);
  return Number.isFinite(size) ? size * 1.2 : 0;
}

/** The selected characters' boxes, one per row run (see the module's comment), in the coordinates
 *  `origin` gives: a client point's `x - origin.x`, `y - origin.y`. */
export function selectionBoxes(range: Range, join: number, origin: { x: number; y: number }): PaintBox[] {
  const doc = range.startContainer.ownerDocument ?? document;
  const parts: Part[] = [];
  const common = range.commonAncestorContainer;
  const walker = doc.createTreeWalker(common, NodeFilter.SHOW_TEXT);
  const piece = doc.createRange();
  for (let n = common.nodeType === Node.TEXT_NODE ? common : walker.nextNode(); n; n = walker.nextNode()) {
    if (!range.intersectsNode(n)) continue;
    const text = n as Text;
    const parent = text.parentElement;
    if ((!/\S/.test(text.data) && parent?.matches(STRUCTURE)) || parent?.closest(NOT_TEXT)) continue;
    const from = text === range.startContainer ? range.startOffset : 0;
    const to = text === range.endContainer ? range.endOffset : text.data.length;
    if (to <= from) continue;
    piece.setStart(text, from);
    piece.setEnd(text, to);
    const cell = parent?.closest("td, th") ?? null;
    const clip = parent?.closest(SCROLLERS)?.getBoundingClientRect() ?? null;
    const line = lineHeightOf(parent);
    for (const r of Array.from(piece.getClientRects?.() ?? [])) {
      const left = Math.max(r.left, clip?.left ?? r.left);
      const right = Math.min(r.right, clip?.right ?? r.right);
      if (right <= left || r.height <= 0) continue;
      parts.push({ top: r.top - origin.y, bottom: r.bottom - origin.y, left: left - origin.x, right: right - origin.x,
        cell, line });
    }
  }
  parts.sort((a, b) => a.top - b.top || a.left - b.left);
  const rows: Part[][] = [];
  let span: { top: number; bottom: number } | null = null;
  for (const p of parts) {
    const overlap = span ? Math.min(span.bottom, p.bottom) - Math.max(span.top, p.top) : -1;
    if (span && overlap >= SAME_ROW * Math.min(span.bottom - span.top, p.bottom - p.top)) {
      rows[rows.length - 1].push(p);
      span = { top: Math.min(span.top, p.top), bottom: Math.max(span.bottom, p.bottom) };
    } else {
      rows.push([p]);
      span = { top: p.top, bottom: p.bottom };
    }
  }
  // Each row's extent, then its share of the leading around it: up to half its line height over its
  // text, never past half the gap to the next row, so the rows of one paragraph meet.
  const extents = rows.map((row) => ({
    top: Math.min(...row.map((p) => p.top)),
    bottom: Math.max(...row.map((p) => p.bottom)),
    line: Math.max(...row.map((p) => p.line)),
  }));
  const out: PaintBox[] = [];
  rows.forEach((row, i) => {
    const e = extents[i];
    const lead = Math.max(0, (e.line - (e.bottom - e.top)) / 2);
    const above = i > 0 ? Math.max(0, (e.top - extents[i - 1].bottom) / 2) : lead;
    const below = i + 1 < extents.length ? Math.max(0, (extents[i + 1].top - e.bottom) / 2) : lead;
    const top = e.top - Math.min(lead, above);
    const bottom = e.bottom + Math.min(lead, below);
    row.sort((a, b) => a.left - b.left);
    let run: Part | null = null;
    for (const p of row) {
      if (run && run.cell === p.cell && p.left - run.right <= join) run.right = Math.max(run.right, p.right);
      else {
        if (run) out.push({ top, bottom, left: run.left, right: run.right });
        run = { ...p };
      }
    }
    if (run) out.push({ top, bottom, left: run.left, right: run.right });
  });
  return out;
}

/** V-LINE's band: from the top of the first selected row to the bottom of the last one (`boxes`, as
 *  `selectionBoxes` gives them), grown to the whole box of a table row the selection starts or ends
 *  in, `width` wide from the list's left edge. */
export function lineBand(range: Range, boxes: PaintBox[], width: number, origin: { x: number; y: number }): PaintBox | null {
  if (boxes.length === 0) return null;
  let top = Math.min(...boxes.map((b) => b.top));
  let bottom = Math.max(...boxes.map((b) => b.bottom));
  for (const node of [range.startContainer, range.endContainer]) {
    const el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
    const row = el?.closest("tr");
    if (!row) continue;
    const r = row.getBoundingClientRect();
    top = Math.min(top, r.top - origin.y);
    bottom = Math.max(bottom, r.bottom - origin.y);
  }
  return { top, bottom, left: 0, right: width };
}

/** Draws the list's selection into `layer` for `mode` (`"visual"` or `"vline"`), or clears it. */
export function paintSelection(list: HTMLElement, layer: HTMLElement, mode: string | null): void {
  const sel = list.ownerDocument.getSelection();
  const range = sel && sel.rangeCount > 0 ? sel.getRangeAt(0) : null;
  if ((mode !== "visual" && mode !== "vline") || !range || !list.contains(range.startContainer) ||
    !list.contains(range.endContainer)) {
    layer.replaceChildren();
    return;
  }
  const box = list.getBoundingClientRect();
  const origin = { x: box.left + list.clientLeft - list.scrollLeft, y: box.top + list.clientTop - list.scrollTop };
  const join = (parseFloat(getComputedStyle(list).fontSize) || 14) / 2;
  const boxes = selectionBoxes(range, join, origin);
  const drawn = mode === "vline" ? [lineBand(range, boxes, list.clientWidth, origin)].filter((b): b is PaintBox => !!b)
    : boxes;
  const scheme = getComputedStyle(list.ownerDocument.documentElement).getPropertyValue("--nv-color-scheme").trim();
  layer.dataset.scheme = scheme === "light" ? "light" : "dark";
  layer.replaceChildren(...drawn.map((b) => {
    const el = list.ownerDocument.createElement("div");
    Object.assign(el.style, { top: `${b.top}px`, left: `${b.left}px`, width: `${b.right - b.left}px`,
      height: `${b.bottom - b.top}px` });
    return el;
  }));
}

/** Keeps `layer` drawn while the list is in VISUAL or V-LINE: after every selection change (each
 *  motion rebuilds the selection), a sideways scroll inside the list, and a resize. Returns the undo. */
export function followSelection(list: HTMLElement, layer: HTMLElement, mode: () => string | null): () => void {
  const paint = () => paintSelection(list, layer, mode());
  // The list's own scroll carries the layer with it; a block scrolled sideways inside it moves its text.
  const nested = (ev: Event) => { if (ev.target !== list) paint(); };
  const doc = list.ownerDocument;
  const view = doc.defaultView;
  doc.addEventListener("selectionchange", paint);
  list.addEventListener("scroll", nested, true);
  view?.addEventListener("resize", paint);
  paint();
  return () => {
    doc.removeEventListener("selectionchange", paint);
    list.removeEventListener("scroll", nested, true);
    view?.removeEventListener("resize", paint);
    layer.replaceChildren();
  };
}
