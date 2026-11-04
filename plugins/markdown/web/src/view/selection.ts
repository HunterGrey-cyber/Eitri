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

/** Text that is drawn but is not the source's text as written: a formula's rendering, a diagram's labels, the box
 *  shown for an image that was not loaded (its reason and alt text). */
const UNALIGNED = ".katex, .mdp-diagram, .katex-mathml, .mdp-box, [aria-hidden='true'], [hidden]";

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

/** A link's or image's destination and optional title, as CommonMark writes it inline. */
const DEST = String.raw`\((?:[^()\s]|\([^()]*\))*(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*\)`;
const IMAGE = new RegExp(String.raw`!\[([^\]]*)\](?:(${DEST})|\[([^\]]*)\])?`, "y");
const TARGET = new RegExp(String.raw`\](?:(${DEST})|\[([^\]]*)\])`, "y");
const COMMENT = /<!--[\s\S]*?(?:-->|$)/y;
const AUTOLINK = /<([A-Za-z][A-Za-z0-9+.-]{1,31}:[^<>\s]*|[^<>\s@]+@[^<>\s@]+)>/y;
const TAG = /<\/?[A-Za-z][A-Za-z0-9-]*(?:\s[^<>]*)?\/?>/y;

/** A reference label as CommonMark matches it: case and runs of white space do not count. */
function label(text: string): string {
  return text.trim().replace(/\s+/g, " ").toLowerCase();
}

/** The reference labels the document defines (`[label]: …`), which decide whether `![alt]` and `[text][label]` draw
 *  as an image and a link or as their own text. */
export function referenceLabels(lines: string[]): Set<string> {
  const out = new Set<string>();
  for (const line of lines) {
    const m = /^ {0,3}\[([^\]^][^\]]*)\]:/.exec(line);
    if (m) out.add(label(m[1]));
  }
  return out;
}

function stickyAt(re: RegExp, text: string, i: number): RegExpExecArray | null {
  re.lastIndex = i;
  return re.exec(text);
}

/** A source line as far as its drawn characters go, read left to right as markdown-it reads inline Markdown: in a
 *  code block (`code`) the whole line, since code draws every character. Elsewhere a reference definition draws
 *  nothing, a footnote definition's label goes, a code span's text stays as written (its backticks go), and what is
 *  written but never drawn goes: an HTML comment, an image with its alt text, a link's target and title, a defined
 *  reference label, anything of an HTML tag's shape. An autolink keeps its address and loses its brackets; an image
 *  or a reference the document does not define draws as text, and stays. Linear in the line's length: backtick runs
 *  are paired through one table, never by searching again. */
export function drawnPart(line: string, code = false, refs: ReadonlySet<string> = new Set()): string {
  if (code) return line;
  if (/^ {0,3}\[[^\]^][^\]]*\]:\s/.test(line)) return "";
  const text = line.replace(/^ {0,3}\[\^[^\]]+\]:/, "");
  // Each backtick run, and the next run of the same length after it: a code span's closing run.
  const runs: { at: number; len: number }[] = [];
  for (let i = 0; i < text.length;) {
    if (text[i] !== "`") { i++; continue; }
    let j = i;
    while (j < text.length && text[j] === "`") j++;
    runs.push({ at: i, len: j - i });
    i = j;
  }
  const closing = new Array<number>(runs.length).fill(-1);
  const seen = new Map<number, number>();
  for (let k = runs.length - 1; k >= 0; k--) {
    closing[k] = seen.get(runs[k].len) ?? -1;
    seen.set(runs[k].len, k);
  }
  const runAt = new Map(runs.map((r, k) => [r.at, k]));
  let out = "";
  let plain = 0;
  let i = 0;
  const take = (to: number, keep: string, next: number) => {
    out += text.slice(plain, to) + keep;
    plain = i = next;
  };
  while (i < text.length) {
    const ch = text[i];
    let m: RegExpExecArray | null;
    if (ch === "`") {
      const k = runAt.get(i)!;
      const c = closing[k];
      if (c >= 0) take(i, text.slice(i + runs[k].len, runs[c].at), runs[c].at + runs[c].len);
      else i += runs[k].len;
    } else if (ch === "<" && (m = stickyAt(COMMENT, text, i))) take(i, "", i + m[0].length);
    else if (ch === "<" && (m = stickyAt(AUTOLINK, text, i))) take(i, m[1], i + m[0].length);
    else if (ch === "<" && (m = stickyAt(TAG, text, i))) take(i, "", i + m[0].length);
    else if (ch === "!" && (m = stickyAt(IMAGE, text, i)) &&
      (m[2] !== undefined || refs.has(label(m[3] !== undefined && m[3] !== "" ? m[3] : m[1])))) {
      take(i, "", i + m[0].length);
    } else if (ch === "]" && (m = stickyAt(TARGET, text, i)) && (m[1] !== undefined || m[2] === "" || refs.has(label(m[2])))) {
      take(i, "]", i + m[0].length);
    } else i++;
  }
  return out + text.slice(plain);
}

/** The source line, in `[from, to)`, of the `k`th (from 0) of a block's rendered characters, white space left out:
 *  each rendered character is looked for in the lines' drawn parts (`drawnPart`) after the one before it, so the
 *  markup between them (`**`, a table's bars, an escape's backslash) is passed over. Line precision is what this
 *  promises. Null when the source runs out first (rendered text that is not the source's, such as an entity). */
function sourceLine(lines: string[], from: number, to: number, shown: string, k: number, code: boolean,
  refs: ReadonlySet<string>): number | null {
  let line = from;
  let at = 0;
  let text = drawnPart(lines[line] ?? "", code, refs);
  const wanted = [...shown];
  for (let j = 0; j <= k && j < wanted.length; j++) {
    let i = text.indexOf(wanted[j], at);
    while (i < 0 && line + 1 < to) {
      line++;
      at = 0;
      text = drawnPart(lines[line] ?? "", code, refs);
      i = text.indexOf(wanted[j]);
    }
    if (i < 0) return null;
    at = i + wanted[j].length;
  }
  return line;
}

/** Whether a block is code, which draws every character of its lines. */
function isCode(box: Box): boolean {
  return box.el.tagName === "PRE";
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
function lineOf(src: SelectionSource, p: Place, first: boolean, refs: ReadonlySet<string>): number | null {
  const index = src.index();
  const box = blockOf(index, p.node);
  if (box) {
    const before = [...shownBefore(box.el, p)].length;
    const all = shownBefore(box.el, null);
    const k = first ? before : before - 1;
    if (k >= 0 && k < [...all].length) {
      const found = sourceLine(src.lines(), box.line, box.end, all, k, isCode(box), refs);
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
  const refs = referenceLabels(src.lines());
  const first = lineOf(src, ends.first, true, refs);
  const last = lineOf(src, ends.last, false, refs);
  if (first === null || last === null) return null;
  const top = Math.min(first, last);
  const bottom = Math.max(first, last);
  const text = sel.toString();
  if (kind === "char" && top === bottom && text !== "" && !text.includes("\n")) {
    const line = src.lines()[top] ?? "";
    const i = line.indexOf(text);
    // Once in the line as written (the columns are taken there) and once in what it draws: a word the line shows
    // only through an entity, while its one written copy is in a link's target, is not the word shown.
    const box = blockOf(src.index(), ends.first.node);
    const drawn = drawnPart(line, box !== null && isCode(box), refs);
    const d = drawn.indexOf(text);
    if (i >= 0 && line.indexOf(text, i + 1) < 0 && d >= 0 && drawn.indexOf(text, d + 1) < 0) {
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
