/** Where a reader who is not following the newest reply is looking, kept across a change of the list's width.
 *
 *  The list keeps `scrollTop` in pixels. A width change reflows every row above the view: a zoom from a panel beside
 *  the editor to the whole window makes the text above a reader deep in a long conversation thousands of pixels
 *  shorter, so the same `scrollTop` shows text pages further on (or the end), and unzooming makes it taller again,
 *  so the reader lands pages earlier. This module records a point in the content, not a pixel offset, and puts it
 *  back at the same height in the view once the new width is laid out.
 *
 *  The point is the cursor's row when any of it is on screen (that is where the reader's eye is), otherwise the
 *  first row on screen. A row whose top is on screen is anchored by its top. A row that starts above the view (a
 *  long reply read in its middle) is anchored by the text at the view's top inside it: the character there, through
 *  a `Range`, whose line is put back at the same height, so the line the reader was on stays the top line. The
 *  character is also kept as its offset in the row's text: a streaming reply's open end is removed and drawn again on
 *  every batch, so the node it was found in can be gone by the time the width changes. The text from the character on
 *  (`CONTEXT` characters) is kept with it, and looked for in the row's text as it is then, a window at a time until
 *  one occurs there exactly once, so a redraw that changed the text before the reader (a reference definition arriving later
 *  turns every earlier `[text][r]` into a link) or in part of what follows still finds the line; with no window found,
 *  the old offset is used. Only where the engine cannot hit-test text, or the row is now shorter than that offset, is
 *  the same fraction of the row's height used instead.
 *
 *  Pure DOM reads and one `scrollTop` write; nothing here runs unless the list calls it. */

/** The geometry this reads, so a test can supply a layout. Defaults to the DOM's own. */
export type AnchorGeometry = {
  rect: (el: Element) => { top: number; bottom: number; left: number; height: number };
  /** The text position at a viewport point, or `null` where the engine has no hit test (jsdom). */
  caretAt: (x: number, y: number) => { node: Node; offset: number } | null;
  /** The viewport top of the line holding a text position, or `null` when it has no box. */
  caretTop: (node: Node, offset: number) => number | null;
};

export type ViewAnchor = {
  row: Element;
  /** The anchored point's height in the view: its viewport top minus the list's top. */
  y: number;
  /** For a row that starts above the view: the point as a fraction of the row's height. */
  fraction: number | null;
  /** For a row that starts above the view: the character at the view's top, when the engine found one, and its
   *  offset in the row's text (`chars`), for when the node itself has been replaced. */
  text: { node: Node; offset: number; chars: number; context: string } | null;
};

type CaretDocument = Document & { caretRangeFromPoint?: (x: number, y: number) => Range | null };

export const domGeometry: AnchorGeometry = {
  rect: (el) => el.getBoundingClientRect(),
  caretAt: (x, y) => {
    const range = (document as CaretDocument).caretRangeFromPoint?.(x, y) ?? null;
    return range === null ? null : { node: range.startContainer, offset: range.startOffset };
  },
  caretTop: (node, offset) => {
    if (node.nodeType !== Node.TEXT_NODE) return null;
    const length = (node.textContent ?? "").length;
    if (length === 0) return null;
    // One character wide: a collapsed range has no client rect on every engine.
    const start = Math.min(offset, length - 1);
    const range = document.createRange();
    range.setStart(node, start);
    range.setEnd(node, start + 1);
    const rects = range.getClientRects();
    return rects.length === 0 ? null : rects[0].top;
  },
};

/** How much text after the reader's character is kept to find it again, and the window it is looked for in. */
const CONTEXT = 64;
const WINDOW = 16;
const STEP = 4;

/** The offset in `row`'s text of a position in one of its text nodes, and the `CONTEXT` characters of the row's text
 *  from there; `null` when the node is not in the row. */
function charsBefore(row: Element, node: Node, offset: number): { chars: number; context: string } | null {
  const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
  let chars = 0;
  for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) {
    if (n !== node) {
      chars += (n.textContent ?? "").length;
      continue;
    }
    let context = (n.textContent ?? "").slice(offset, offset + CONTEXT);
    for (let m = walker.nextNode(); m !== null && context.length < CONTEXT; m = walker.nextNode()) {
      context += (m.textContent ?? "").slice(0, CONTEXT - context.length);
    }
    return { chars: chars + offset, context };
  }
  return null;
}

/** Where the kept character is in `text` now: the first window of its context (one every `STEP` characters) that
 *  occurs exactly once there, less that window's distance from the character. A window found more than once says
 *  nothing (a reply repeats phrases), so it is passed over. `null` when no window is found once. */
function relocate(text: string, context: string): number | null {
  for (let d = 0; d + WINDOW <= context.length; d += STEP) {
    const window = context.slice(d, d + WINDOW);
    const at = text.indexOf(window);
    if (at !== -1 && text.indexOf(window, at + 1) === -1) return Math.max(0, at - d);
  }
  return null;
}

/** The text position at `chars` into `row`'s text, or `null` past its end. */
function positionAt(row: Element, chars: number): { node: Node; offset: number } | null {
  const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
  let left = chars;
  for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) {
    const length = (n.textContent ?? "").length;
    if (left < length) return { node: n, offset: left };
    left -= length;
  }
  return null;
}

/** The first row whose bottom is below the list's top: rows are stacked in order, so a binary search finds it. */
function firstOnScreen(rows: readonly Element[], top: number, geometry: AnchorGeometry): number {
  let lo = 0;
  let hi = rows.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (geometry.rect(rows[mid]).bottom <= top) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/** Records where the reader is looking; `null` when no row is on screen. `cursorRow` is the cursor's row, if any. */
export function captureAnchor(
  list: HTMLElement,
  rows: readonly Element[],
  cursorRow: Element | undefined,
  geometry: AnchorGeometry = domGeometry,
): ViewAnchor | null {
  const l = geometry.rect(list);
  const bottom = l.top + list.clientHeight;
  let row: Element | undefined;
  if (cursorRow !== undefined) {
    const r = geometry.rect(cursorRow);
    if (r.bottom > l.top && r.top < bottom) row = cursorRow;
  }
  if (row === undefined) {
    const index = firstOnScreen(rows, l.top, geometry);
    if (index >= rows.length || geometry.rect(rows[index]).top >= bottom) return null;
    row = rows[index];
  }
  const r = geometry.rect(row);
  if (r.top >= l.top) return { row, y: r.top - l.top, fraction: null, text: null };
  // The row starts above the view: the character at the view's top, a little inside the row's left edge.
  const probe = l.top + 1;
  const caret = geometry.caretAt(r.left + 8, probe);
  let text: ViewAnchor["text"] = null;
  let y = 0;
  if (caret !== null && row.contains(caret.node)) {
    const top = geometry.caretTop(caret.node, caret.offset);
    const found = top === null ? null : charsBefore(row, caret.node, caret.offset);
    if (top !== null && found !== null) {
      text = { node: caret.node, offset: caret.offset, chars: found.chars, context: found.context };
      y = top - l.top;
    }
  }
  const fraction = r.height > 0 ? (l.top - r.top) / r.height : 0;
  return { row, y, fraction, text };
}

/** How far the anchored point now sits from where it was, in the view; `null` when its row is gone. */
export function anchorDrift(list: HTMLElement, anchor: ViewAnchor, geometry: AnchorGeometry = domGeometry): number | null {
  if (!anchor.row.isConnected || !list.contains(anchor.row)) return null;
  const l = geometry.rect(list);
  const r = geometry.rect(anchor.row);
  if (anchor.text !== null) {
    const kept = anchor.text.node.isConnected && anchor.row.contains(anchor.text.node);
    const moved = kept ? null : relocate(anchor.row.textContent ?? "", anchor.text.context);
    const at = kept ? anchor.text : positionAt(anchor.row, moved ?? anchor.text.chars);
    const top = at === null ? null : geometry.caretTop(at.node, at.offset);
    if (top !== null) return top - l.top - anchor.y;
  }
  if (anchor.fraction !== null) return r.top + anchor.fraction * r.height - l.top;
  return r.top - l.top - anchor.y;
}

/** Puts the anchored point back where it was in the view. Returns `scrollTop` after the write (the engine clamps
 *  it at either end), or `null` when the anchor's row is gone and nothing was written. */
export function restoreAnchor(list: HTMLElement, anchor: ViewAnchor, geometry: AnchorGeometry = domGeometry): number | null {
  const drift = anchorDrift(list, anchor, geometry);
  if (drift === null) return null;
  const rounded = Math.round(drift);
  if (rounded !== 0) list.scrollTop = list.scrollTop + rounded;
  return list.scrollTop;
}
