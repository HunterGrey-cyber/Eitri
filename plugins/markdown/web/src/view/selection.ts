import type { Box } from "./lineindex";

/** What the source of a selection is: its two ends in nvim's terms (lines from 0, columns in characters from 1; 0 for
 *  a linewise end) and nvim's visual mode for it. The anchor is where the selection started, the focus where its
 *  cursor is. */
export interface SourceSelection {
  mode: "char" | "line";
  anchor: { line: number; col: number };
  focus: { line: number; col: number };
}

/** What the page tells the mapping: its line index, the lines it rendered, and the source line at a document y. */
export interface SelectionSource {
  index(): Box[];
  lines(): string[];
  sourceLineAt(y: number): number | null;
}

/** Text that is drawn but is not the source's text as written: a formula's rendering, a diagram's labels. */
const UNALIGNED = ".katex, .mdp-diagram, .katex-mathml, [aria-hidden='true'], [hidden]";

type Place = { node: Node; offset: number };

/** The rendered characters of `block` before `end` (all of them without `end`), white space left out: the text a
 *  reader sees, in order. */
function shownBefore(block: Element, end: Place | null): string {
  let out = "";
  const walker = block.ownerDocument.createTreeWalker(block, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const text = n as Text;
    const at = end && text === end.node ? end.offset : null;
    if (end && at === null) {
      // A place given as an element and a child offset: the text nodes after that child are after the place.
      const range = block.ownerDocument.createRange();
      range.setStart(end.node, end.offset);
      if (range.comparePoint(text, 0) >= 0) break;
    }
    if (!text.parentElement?.closest(UNALIGNED)) out += (at === null ? text.data : text.data.slice(0, at));
    if (at !== null) break;
  }
  return out.replace(/\s+/g, "");
}

/** A source line as far as its drawn characters go: what Markdown writes but never draws taken out, so a rendered
 *  character is never matched inside it (a link's target, a reference label, an HTML tag, a reference definition,
 *  which draws nothing at all). A footnote definition's label goes, its text stays. Only the line this leaves is used,
 *  never a column. */
export function drawnPart(line: string): string {
  return line
    .replace(/^ {0,3}\[\^[^\]]+\]:/, "")
    .replace(/^ {0,3}\[[^\]]+\]:\s.*$/, "")
    .replace(/\]\((?:[^()\s]|\([^()]*\))*(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*\)/g, "]")
    .replace(/\]\[[^\]]*\]/g, "]")
    .replace(/<\/?[A-Za-z][^<>]*>/g, "");
}

/** The source line, in `[from, to)`, of the `k`th (from 0) of a block's rendered characters, white space left out:
 *  each rendered character is looked for in the lines' drawn parts (`drawnPart`) after the one before it, so the
 *  markup between them (`**`, a table's bars, an escape's backslash) is passed over. Line precision is what this
 *  promises. Null when the source runs out first (rendered text that is not the source's, such as an entity). */
function sourceLine(lines: string[], from: number, to: number, shown: string, k: number): number | null {
  let line = from;
  let at = 0;
  let text = drawnPart(lines[line] ?? "");
  const wanted = [...shown];
  for (let j = 0; j <= k && j < wanted.length; j++) {
    let i = text.indexOf(wanted[j], at);
    while (i < 0 && line + 1 < to) {
      line++;
      at = 0;
      text = drawnPart(lines[line] ?? "");
      i = text.indexOf(wanted[j]);
    }
    if (i < 0) return null;
    at = i + wanted[j].length;
  }
  return line;
}

/** The index block holding `node`, if any. */
function blockOf(index: Box[], node: Node): Box | null {
  const el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
  if (!el) return null;
  for (const b of index) if (b.el.contains(el)) return b;
  return null;
}

/** The middle y, in document coordinates, of the character after (`after`) or before a place. */
function placeY(p: Place, after: boolean): number | null {
  const doc = p.node.ownerDocument ?? document;
  const range = doc.createRange();
  if (p.node.nodeType === Node.TEXT_NODE) {
    const length = (p.node as Text).data.length;
    const at = after ? Math.min(p.offset, Math.max(0, length - 1)) : Math.max(0, p.offset - 1);
    range.setStart(p.node, at);
    range.setEnd(p.node, Math.min(at + 1, length));
  } else range.selectNodeContents(p.node.childNodes[Math.min(p.offset, p.node.childNodes.length - 1)] ?? p.node);
  const rects = range.getClientRects?.() ?? [];
  const rect = rects.length ? rects[after ? 0 : rects.length - 1] : null;
  return rect ? (rect.top + rect.bottom) / 2 + window.scrollY : null;
}

/** The source line of one end of a selection: the first selected character's (`first`) or the last one's. Inside a
 *  block, read off the source by its rendered text; elsewhere (or where that fails) the line at its height. */
function lineOf(src: SelectionSource, p: Place, first: boolean): number | null {
  const index = src.index();
  const box = blockOf(index, p.node);
  if (box) {
    const before = [...shownBefore(box.el, p)].length;
    const all = shownBefore(box.el, null);
    const k = first ? before : before - 1;
    if (k >= 0 && k < [...all].length) {
      const found = sourceLine(src.lines(), box.line, box.end, all, k);
      if (found !== null) return found;
    }
  }
  const y = placeY(p, first);
  return y === null ? null : src.sourceLineAt(y);
}

/** The places just before the first selected character that is not white space and just after the last one: a
 *  selection's own ends may sit in white space or at the edge of the next block (a whole line ends where the next
 *  line's block starts). Null when nothing but white space is selected. */
function selectedEnds(range: Range): { first: Place; last: Place } | null {
  const doc = range.startContainer.ownerDocument ?? document;
  const common = range.commonAncestorContainer;
  const root = common.nodeType === Node.TEXT_NODE ? common.parentNode ?? common : common;
  let first: Place | null = null;
  let last: Place | null = null;
  const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (!range.intersectsNode(n)) continue;
    const text = n as Text;
    const from = text === range.startContainer ? range.startOffset : 0;
    const to = text === range.endContainer ? range.endOffset : text.data.length;
    const part = text.data.slice(from, to);
    const lead = part.search(/\S/);
    if (lead < 0) continue;
    first ??= { node: text, offset: from + lead };
    last = { node: text, offset: from + part.replace(/\s+$/, "").length };
  }
  return first && last ? { first, last } : null;
}

/** The source of the page's selection `sel`, for nvim's visual mode `kind`: charwise when the selection lies inside
 *  one source line and its text occurs there exactly once (the text as shown: rendered Markdown does not map back to
 *  source columns otherwise), else linewise over the source lines it spans. Null with no selection. */
export function sourceSelection(src: SelectionSource, sel: Selection, kind: "char" | "line"): SourceSelection | null {
  if (!sel.anchorNode || !sel.focusNode || sel.rangeCount === 0) return null;
  const anchor = { node: sel.anchorNode, offset: sel.anchorOffset };
  const focus = { node: sel.focusNode, offset: sel.focusOffset };
  let forward = true;
  try {
    const range = (anchor.node.ownerDocument ?? document).createRange();
    range.setStart(anchor.node, anchor.offset);
    forward = range.comparePoint(focus.node, focus.offset) >= 0;
  } catch {
    forward = true;
  }
  const ends = selectedEnds(sel.getRangeAt(0));
  if (!ends) return null;
  const first = lineOf(src, ends.first, true);
  const last = lineOf(src, ends.last, false);
  if (first === null || last === null) return null;
  const top = Math.min(first, last);
  const bottom = Math.max(first, last);
  const text = sel.toString();
  if (kind === "char" && top === bottom && text !== "" && !text.includes("\n")) {
    const line = src.lines()[top] ?? "";
    const i = line.indexOf(text);
    if (i >= 0 && line.indexOf(text, i + 1) < 0) {
      const from = [...line.slice(0, i)].length + 1;
      const to = from + [...text].length - 1;
      return forward
        ? { mode: "char", anchor: { line: top, col: from }, focus: { line: top, col: to } }
        : { mode: "char", anchor: { line: top, col: to }, focus: { line: top, col: from } };
    }
  }
  return forward
    ? { mode: "line", anchor: { line: top, col: 0 }, focus: { line: bottom, col: 0 } }
    : { mode: "line", anchor: { line: bottom, col: 0 }, focus: { line: top, col: 0 } };
}
