import { FENCE_ATTR, RAW_HTML_ATTR } from "../render/env";

/** One block of the rendered document and the source lines it stands for, in document coordinates. */
export interface Box { line: number; end: number; top: number; height: number; el: Element }

/** What `measure` read for one tagged element, before the index chooses among them. */
export interface Measured {
  el: Element; line: number; end: number | null; top: number; height: number;
  visible: boolean; hasTaggedVisibleDescendant: boolean;
}

/** The index the sync maps through: visible leaf blocks only (a container's lines are carried by the blocks inside
 *  it, and a zero-height marker would pull the mapping to a point), each with an end line, in top order with lines
 *  strictly rising, so both binary searches hold; of two entries for one line the later, innermost one stays. A block
 *  shown below a block of a later line (a footnote definition is rendered at the end but keeps its own line) is left
 *  out: the sync maps across it as across a gap. */
export function buildIndex(items: Measured[]): Box[] {
  const kept = items
    .filter((m) => m.visible && m.height > 0 && !m.hasTaggedVisibleDescendant)
    .map((m, order) => ({
      order,
      box: { line: m.line, end: m.end !== null && m.end > m.line ? m.end : m.line + 1, top: m.top, height: m.height, el: m.el },
    }));
  kept.sort((a, b) => a.box.top - b.box.top || a.order - b.order);
  const out: Box[] = [];
  for (const { box } of kept) {
    const last = out[out.length - 1];
    if (last && last.line === box.line) out[out.length - 1] = box;
    else if (!last || box.line > last.line) out.push(box);
  }
  return out;
}

function lineAttr(value: string | null): number | null {
  if (value === null) return null;
  const n = Number(value);
  return Number.isInteger(n) && n >= 0 ? n : null;
}

/* Hidden by a closed details above it, unless it sits in that details' own summary (which stays visible). */
function inClosedDetails(el: Element, root: Element): boolean {
  for (let p = el.parentElement; p && p !== root; p = p.parentElement) {
    if (p.tagName === "DETAILS" && !p.hasAttribute("open")) {
      const summary = p.querySelector(":scope > summary");
      if (!summary || !summary.contains(el)) return true;
    }
  }
  return false;
}

/** Every `.code-line` under `root` in document order, with its lines and its box in document coordinates
 *  (`scrollY` added to the viewport's). A code block's box is its content box: its padding is not source. */
export function measure(root: Element, scrollY: number): Measured[] {
  const out: Measured[] = [];
  const byEl = new Map<Element, Measured>();
  // The sanitiser keeps these boundaries only when the renderer marked them with its nonce. The HTML between
  // them supplies geometry alone; its attributes never supply source lines or a boundary.
  const htmlBoxes = new Map<Element, { top: number; height: number }>();
  const starts = new Map<string, Element>();
  for (const el of root.querySelectorAll(`[${RAW_HTML_ATTR}]`)) {
    const boundary = /^(start|end):(\d+)$/.exec(el.getAttribute(RAW_HTML_ATTR) ?? "");
    if (!boundary) continue;
    const [, kind, ordinal] = boundary;
    if (kind === "start") { starts.set(ordinal, el); continue; }
    const start = starts.get(ordinal);
    if (!start) continue;
    starts.delete(ordinal);
    const range = root.ownerDocument.createRange();
    range.setStartAfter(start);
    range.setEndBefore(el);
    let top = Number.POSITIVE_INFINITY;
    let bottom = Number.NEGATIVE_INFINITY;
    for (const rect of Array.from(range.getClientRects?.() ?? [])) {
      if (rect.height <= 0) continue;
      top = Math.min(top, rect.top);
      bottom = Math.max(bottom, rect.bottom);
    }
    if (bottom > top) htmlBoxes.set(start, { top, height: bottom - top });
  }
  for (const el of root.querySelectorAll(".code-line")) {
    const line = lineAttr(el.getAttribute("data-line"));
    if (line === null) continue;
    const rect = htmlBoxes.get(el) ?? el.getBoundingClientRect();
    let top = rect.top + scrollY;
    let height = rect.height;
    if (el.tagName === "PRE") {
      const style = getComputedStyle(el);
      const padTop = parseFloat(style.paddingTop) || 0;
      const padBottom = parseFloat(style.paddingBottom) || 0;
      top += padTop;
      height = Math.max(0, height - padTop - padBottom);
    }
    let first = line;
    let end = lineAttr(el.getAttribute("data-line-end"));
    // A fenced block's rows are its code: its lines are the ones after the opening fence, and the fence lines fall
    // into the padding around them, as the gaps between blocks do.
    const code = lineAttr(el.getAttribute(FENCE_ATTR));
    if (el.tagName === "PRE" && code !== null && code > 0 && (end === null || line + 1 + code <= end)) {
      first = line + 1;
      end = line + 1 + code;
    }
    const m: Measured = {
      el, line: first, end, top, height,
      visible: height > 0 && !inClosedDetails(el, root), hasTaggedVisibleDescendant: false,
    };
    out.push(m);
    byEl.set(el, m);
  }
  for (const m of out) {
    if (!m.visible) continue;
    for (let p = m.el.parentElement; p && p !== root; p = p.parentElement) {
      const container = byEl.get(p);
      if (container) container.hasTaggedVisibleDescendant = true;
    }
  }
  return out;
}

/** The position of the last entry whose line is at or before `line`, or -1. */
export function lastLineAtOrBefore(index: Box[], line: number): number {
  let lo = 0;
  let hi = index.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (index[mid].line <= line) { found = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return found;
}

/** The position of the last entry whose top is at or above `y` (`buildIndex` keeps the index in top order as well as
 *  line order), or -1. */
export function lastTopAtOrBefore(index: Box[], y: number): number {
  let lo = 0;
  let hi = index.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (index[mid].top <= y) { found = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return found;
}
