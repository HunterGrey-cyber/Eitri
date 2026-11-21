import { lastTopAtOrBefore, type Box } from "./lineindex";

/** One rendered row of the page in document coordinates: a line box of the text (a paragraph's wrapped line, a
 *  list item's line, a heading, a code line, a table row's line), or a block with no text rows (an image, a diagram,
 *  a rule, display math) as one row. */
export interface Row { top: number; bottom: number }

/** What is read as one box, never as the text inside it: a formula's spans sit at several heights (a fraction's
 *  numerator and denominator are not two rows), a diagram's labels are drawn, not read, and an image or a rule has
 *  no text at all. */
export const ATOM = "img, svg, hr, video, canvas, .katex, .katex-display, .mdp-diagram";
/** What has no box of its own to read, and no text of the source's that the page shows: KaTeX's MathML copy is
 *  clipped to a pixel, and the rest is never drawn. */
export const SKIP = ".katex-mathml, [hidden], [aria-hidden='true'], script, style, template";

/** How much of the shorter of two boxes must overlap the other, vertically, for both to sit on one row: a
 *  superscript, an inline formula or an inline image taller than the text shares the line it rides on. */
const SAME_ROW = 0.5;

/** The line fragments of each index block, read once for the line index (the layout) they belong to: the cursor
 *  passing a long block again and again (a fenced code block of hundreds of lines is one block) reads it once. */
export class RowCache {
  readonly blocks: Set<Element>;
  readonly rows = new Map<Element, Row[]>();
  constructor(readonly index: Box[]) { this.blocks = new Set(index.map((b) => b.el)); }
}

/* The vertical extents of every line fragment of text and every atom in document order from `first` to `last`
   (both included, with everything between them, inside or outside the index's blocks: a list item's own text above
   its nested list is in no index block, and it still has rows). An index block's fragments come from the cache, read
   on its first visit. */
function extents(root: Element, first: Element, last: Element, scrollY: number, cache: RowCache): Row[] {
  const doc = root.ownerDocument;
  const range = doc.createRange();
  const read = (out: Row[], r: { top: number; bottom: number; height: number }) => {
    if (r.height > 0) out.push({ top: r.top + scrollY, bottom: r.bottom + scrollY });
  };
  // Walks `from`'s subtree (or, with `until`, on from `start` to `until`), text nodes only; an atom is read as it is
  // passed, nothing under it or under a skipped element is visited, and an index block (but `from`) is taken whole.
  const walk = (out: Row[], from: Node, start: Node, until: Element | null, blocks: boolean) => {
    const walker = doc.createTreeWalker(from, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {
      acceptNode(node: Node): number {
        if (node.nodeType !== Node.ELEMENT_NODE) return NodeFilter.FILTER_ACCEPT;
        const el = node as Element;
        if (el.matches(SKIP)) return NodeFilter.FILTER_REJECT;
        if (el.matches(ATOM)) { read(out, el.getBoundingClientRect()); return NodeFilter.FILTER_REJECT; }
        if (blocks && cache.blocks.has(el)) { out.push(...block(el)); return NodeFilter.FILTER_REJECT; }
        return NodeFilter.FILTER_SKIP;
      },
    });
    walker.currentNode = start;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (until && !until.contains(node) && (until.compareDocumentPosition(node) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0) {
        break;
      }
      if (!(node.nodeValue ?? "").trim()) continue;
      range.selectNodeContents(node);
      const rects = typeof range.getClientRects === "function" ? range.getClientRects() : [];
      for (const r of Array.from(rects)) read(out, r);
    }
  };
  // One index block's fragments: an atom block (a diagram) is one box; else its text and atoms.
  const block = (el: Element): Row[] => {
    const known = cache.rows.get(el);
    if (known) return known;
    const own: Row[] = [];
    if (el.matches(ATOM)) read(own, el.getBoundingClientRect());
    else if (!el.matches(SKIP)) walk(own, el, el, null, false);
    cache.rows.set(el, own);
    return own;
  };
  const out: Row[] = [];
  // `first` is where the walk starts, so the filter never sees it: a block is taken whole here, and the walk goes on
  // after its last descendant.
  let start: Node = first;
  if (first !== root) {
    if (cache.blocks.has(first)) out.push(...block(first));
    else if (first.matches(ATOM)) read(out, first.getBoundingClientRect());
    if (cache.blocks.has(first) || first.matches(ATOM) || first.matches(SKIP)) {
      while (start.lastChild) start = start.lastChild;
    }
  }
  if (first === last && cache.blocks.has(first)) return out;
  walk(out, root, start, last, true);
  range.detach?.();
  return out;
}

/** Folds line fragments into rows: fragments that overlap by `SAME_ROW` of the shorter one are one row. Each row is
 *  then given its share of the leading around it (up to half a line of `line` px over its text, never past half the
 *  gap to the next row), so the rows of a paragraph meet and a band drawn over one covers its line box. */
export function foldRows(parts: Row[], line: number): Row[] {
  const sorted = [...parts].sort((a, b) => a.top - b.top || a.bottom - b.bottom);
  const rows: Row[] = [];
  for (const p of sorted) {
    const last = rows[rows.length - 1];
    if (last) {
      const overlap = Math.min(last.bottom, p.bottom) - Math.max(last.top, p.top);
      const shorter = Math.min(last.bottom - last.top, p.bottom - p.top);
      if (overlap >= SAME_ROW * shorter) { last.top = Math.min(last.top, p.top); last.bottom = Math.max(last.bottom, p.bottom); continue; }
    }
    rows.push({ top: p.top, bottom: p.bottom });
  }
  const grown = rows.map((r) => ({ ...r }));
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i];
    const lead = Math.max(0, (line - (r.bottom - r.top)) / 2);
    const above = i > 0 ? Math.max(0, (r.top - rows[i - 1].bottom) / 2) : lead;
    const below = i + 1 < rows.length ? Math.max(0, (rows[i + 1].top - r.bottom) / 2) : lead;
    grown[i].top = r.top - Math.min(lead, above);
    grown[i].bottom = r.bottom + Math.min(lead, below);
  }
  return grown;
}

/**
 * The rows of the rendered document between document y `lo` and `hi` (and the rows that reach into that span), from
 * the index's blocks there and one more on each side: the work is the text around the span, not the document. `line`
 * is the root's line height, the leading a row is given.
 */
export function rowsIn(
  root: Element, index: Box[], lo: number, hi: number, line: number, scrollY: number, cache = new RowCache(index),
): Row[] {
  if (index.length === 0) return [];
  const a = Math.max(0, lastTopAtOrBefore(index, lo) - 1);
  let b = Math.max(a, lastTopAtOrBefore(index, hi));
  b = Math.min(index.length - 1, b + 1);
  // From the first block, the walk starts at the root: text before the first index block (a list item's own line
  // above the nested list that holds the first block) has rows too. Fragments more than two lines outside the span
  // cannot change a row inside it (its leading reaches half a line at most), so they are not folded.
  const near = (r: Row) => r.bottom > lo - 2 * line && r.top < hi + 2 * line;
  return foldRows(extents(root, a === 0 ? root : index[a].el, index[b].el, scrollY, cache).filter(near), line)
    .filter((r) => r.bottom > lo && r.top < hi);
}
