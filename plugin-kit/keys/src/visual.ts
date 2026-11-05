/*! Visual mode is ported from Vimium's visual mode (content_scripts/mode_visual.js):
 * Copyright (c) 2010 Phil Crosby, Ilya Sukhar. MIT License; the full notice is in LICENSE-vimium. */
import { onRemove } from "./bridge";
import { kitMessage, matchFrom } from "./find";
import { scrollToAnimated, windowDestination } from "./glide";
import { showMessage } from "./hud";
import { parseNotation, type KeyStroke } from "./notation";
import { createResolver, type Resolution } from "./resolver";
import { currentSettings } from "./settings";
import type { KitMessage, Row, VisualKind, VisualOptions } from "./index";

type Direction = "forward" | "backward";
type Granularity = "character" | "word" | "line" | "lineboundary";
type Alterable = Selection & { modify(alter: "extend" | "move", direction: Direction, granularity: Granularity): void };
/** A place in the text: just before a character, as a caret stands. */
type Place = { node: Node; offset: number };

/** A word character for `w`, `b` and `e`, as vim's 'iskeyword' reads letters, digits and `_` in every script. */
const WORD = /[\p{L}\p{N}_]/u;
/** The kit's own elements, and text nobody reads as the page's: a selection never reaches into them. */
const NOT_PAGE = "#eitri-keys-hud, #eitri-keys-sheet, #eitri-keys-find, #eitri-keys-hints, [aria-hidden='true']";
/** How far from the view's edges the cursor is kept on a page that does not place it itself. */
const MARGIN_SHARE = 1 / 8;

/** Visual mode's motions, vim's: fixed, as hint mode's letters are, so they are never declared. */
const MOTIONS = ["h", "l", "j", "k", "w", "b", "e", "0", "$", "gg", "G", "n", "N"];
const COMMANDS = ["o", "y", "v", "V", "<C-e>", "<C-y>", "<Esc>"];

let kind: VisualKind | null = null;
/** vim's two ends: the anchor's character (where the mode started, or the far end after `o`) and the cursor's,
 *  each as the place just before it. The selection drawn holds both characters and everything between them
 *  (linewise: their whole lines), as vim's does, whichever comes first. */
let anchor: Place | null = null;
let cursor: Place | null = null;
let options: VisualOptions & {
  root?: () => Element | null;
  messages?: Partial<Record<KitMessage, string>>;
  /** Told when the mode ends, so the HUD shows it. */
  ended?: () => void;
} = {};
let resolver: ReturnType<typeof createResolver> | null = null;
let pending = "";

/** While visual mode runs the root element says which (`data-eitri-visual`), so the kit's stylesheet draws the
 *  selection in the editor's Visual colour and a page can drop what it draws over the text. */
function mark(next: VisualKind | null): void {
  if (next) document.documentElement.setAttribute("data-eitri-visual", next);
  else document.documentElement.removeAttribute("data-eitri-visual");
}

/** The highlight the selection is drawn as, where the engine has the Custom Highlight API. */
const HIGHLIGHT = "eitri-visual";
type Registry = { set(name: string, value: unknown): unknown; delete(name: string): unknown };

function registry(): { highlights: Registry; make: () => { add(range: Range): unknown } } | null {
  const css = (globalThis as { CSS?: { highlights?: Registry } }).CSS;
  const ctor = (globalThis as { Highlight?: new () => { add(range: Range): unknown } }).Highlight;
  return css?.highlights && typeof ctor === "function" ? { highlights: css.highlights, make: () => new ctor() } : null;
}

/* The selection is drawn as a highlight of its text, and the engine's own selection painting is turned off
   (`data-eitri-visual-paint`): WebKit paints a selection with gaps, filling a table cell's padding up to the next
   selected cell and a block's width past the end of its text, so a row of cells came out as boxes of different
   widths and heights, slivers above the text and a cell left out. A highlight paints the selected characters and
   nothing else. Without the API the engine's selection stays, in the Visual colour. WebKitGTK 2.54 paints no
   highlight on text that sits in an anonymous block (a list item's own text beside its nested list): a page whose
   text can be laid out that way draws the selection itself and hides this one. */
function paint(s: Selection | null): void {
  const api = registry();
  if (!api || !kind || !s || s.rangeCount === 0) {
    unpaint();
    return;
  }
  const highlight = api.make();
  highlight.add(s.getRangeAt(0).cloneRange());
  api.highlights.set(HIGHLIGHT, highlight);
  document.documentElement.setAttribute("data-eitri-visual-paint", "highlight");
}

function unpaint(): void {
  registry()?.highlights.delete(HIGHLIGHT);
  document.documentElement.removeAttribute("data-eitri-visual-paint");
}

onRemove(() => {
  if (kind) document.getSelection()?.removeAllRanges();
  unpaint();
  mark(null);
  kind = null;
  anchor = null;
  cursor = null;
  options = {};
  resolver = null;
  pending = "";
});

/** What the page told `install` about visual mode, the find root the selection is kept inside, and the messages. */
export function configureVisual(next: typeof options): void {
  options = next;
  resolver = null;
}

/** The visual mode running, or null. A mode whose text went (the page re-rendered it, or the selection was
 *  dropped) is ended first, so the HUD and `data-eitri-visual` never outlive it. */
export function visualKind(): VisualKind | null {
  if (kind && !standing()) endVisual();
  return kind;
}

/** Whether the mode's selection and both its ends are still on the page's text. */
function standing(): boolean {
  const s = document.getSelection();
  return !!s && s.rangeCount > 0 && !!anchor && !!cursor && anchor.node.isConnected && cursor.node.isConnected &&
    onPage(anchor.node) && onPage(cursor.node) && exists(anchor) && exists(cursor);
}

/** The keys typed so far of a visual-mode key that waits for more (`g`). */
export function visualPending(): string {
  return pending;
}

function selection(): Alterable | null {
  const s = document.getSelection() as Alterable | null;
  return s && typeof s.modify === "function" ? s : null;
}

function rootOf(): Element {
  let root: Element | null = null;
  try {
    root = options.root?.() ?? null;
  } catch (error) {
    console.warn("eitri keys: the page's find root failed", error);
  }
  return root ?? document.body;
}

/** Whether a place is the page's text: inside the root and outside the kit's own elements. */
function onPage(node: Node | null): boolean {
  if (!node) return false;
  const el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
  return !!el && rootOf().contains(el) && !el.closest(NOT_PAGE);
}

/** Below 0 when `a` comes before `b`, 0 at the same place, above 0 after it. */
function order(a: Place, b: Place): number {
  try {
    const range = document.createRange();
    range.setStart(b.node, b.offset);
    return range.comparePoint(a.node, a.offset);
  } catch {
    return 0;
  }
}

/** Whether a place still exists: its node's text (or children) can have shrunk under it in an update. */
function exists(p: Place): boolean {
  const max = p.node.nodeType === Node.TEXT_NODE ? (p.node as Text).data.length : p.node.childNodes.length;
  return p.offset <= max;
}

function focusOf(s: Selection): Place | null {
  return s.focusNode ? { node: s.focusNode, offset: s.focusOffset } : null;
}

/* Movement, as Vimium's: the selection's own `modify`, with word moves done a character at a time so they behave
   the same on every platform. Each runs from a caret at a place and gives the place it reaches. */

/** The character after the focus (a whole code point: a letter outside the BMP is two UTF-16 units), the selection
 *  left as it was. */
function nextCharacter(s: Alterable): string | undefined {
  const before = s.toString();
  s.modify("extend", "forward", "character");
  const after = s.toString();
  if (before === after) return undefined;
  s.modify("extend", "backward", "character");
  return [...after.slice(before.length)][0];
}

function nextIsWord(s: Alterable): boolean {
  const ch = nextCharacter(s);
  return ch !== undefined && WORD.test(ch);
}

/** Extends by one character; how much longer the selection got (0 at the end of the text). */
function extendOne(s: Alterable, direction: Direction): number {
  const length = s.toString().length;
  s.modify("extend", direction, "character");
  return s.toString().length - length;
}

/** Where `run` takes a caret standing at `from`. */
function from(s: Alterable, at: Place, run: (s: Alterable) => void): Place {
  s.collapse(at.node, at.offset);
  run(s);
  return focusOf(s) ?? at;
}

/** The place one character on from `at` (back with `backward`), or `at` at the text's end. */
function step(s: Alterable, at: Place, direction: Direction): Place {
  return from(s, at, (sel) => sel.modify("extend", direction, "character"));
}

/** The cursor's next place for a motion, vim's: the cursor stands on a character, so a motion that ends after one
 *  (`e`, `$`) comes back one to stand on it. */
function moveCursor(s: Alterable, at: Place, motion: string): Place | null {
  switch (motion) {
    case "h": return step(s, at, "backward");
    case "l": return step(s, at, "forward");
    case "j": return rowMove(at, "forward") ?? from(s, at, (sel) => sel.modify("extend", "forward", "line"));
    case "k": return rowMove(at, "backward") ?? from(s, at, (sel) => sel.modify("extend", "backward", "line"));
    case "b": return from(s, at, (sel) => sel.modify("extend", "backward", "word"));
    case "0": return from(s, at, (sel) => sel.modify("extend", "backward", "lineboundary"));
    case "w":
      // To the end of this word, then over what follows it up to the next word.
      return from(s, at, (sel) => {
        while (nextIsWord(sel)) if (extendOne(sel, "forward") === 0) return;
        while (nextCharacter(sel) !== undefined && !nextIsWord(sel)) if (extendOne(sel, "forward") === 0) return;
      });
    case "e": {
      // Past the cursor's own character, over what is not a word, to the end of the next word's last character.
      const end = from(s, at, (sel) => {
        extendOne(sel, "forward");
        while (nextCharacter(sel) !== undefined && !nextIsWord(sel)) if (extendOne(sel, "forward") === 0) return;
        while (nextIsWord(sel)) if (extendOne(sel, "forward") === 0) return;
      });
      return order(end, at) > 0 ? step(s, end, "backward") : at;
    }
    case "$": {
      const end = from(s, at, (sel) => sel.modify("extend", "forward", "lineboundary"));
      return order(end, at) > 0 ? step(s, end, "backward") : at;
    }
    case "gg": case "G": {
      const all = texts(rootOf());
      const text = motion === "G" ? all[all.length - 1] : all[0];
      if (!text) return null;
      return motion === "G" ? step(s, { node: text, offset: text.data.length }, "backward") : { node: text, offset: 0 };
    }
    case "n": case "N": {
      const match = matchFrom(at.node, at.offset, motion === "n" ? 1 : -1);
      return match ? { node: match.startContainer, offset: match.startOffset } : null;
    }
    default: return null;
  }
}

/** The offset of a text's last character (a whole code point: a letter outside the BMP is two UTF-16 units). */
function lastCharOffset(t: Text): number {
  const points = [...t.data];
  return Math.max(0, t.data.length - (points[points.length - 1]?.length ?? 1));
}

/** The cells of a table row, in order. */
function cellsOf(row: Element): Element[] {
  return [...row.children].filter((c) => c.matches("td, th"));
}

/** `j` and `k` from a table row. The engine's line move goes from a cell to the next cell of the same row (each cell
 *  is a block of its own), so the cursor goes to the row below (above) instead: the cell in the same column, at the
 *  same character or the cell's last; past the table's last (first) row, to the first text after the table (the
 *  last character before it). Null outside a table. */
function rowMove(at: Place, direction: Direction): Place | null {
  const row = rowOf(at.node);
  const table = row?.closest("table");
  if (!row || !table) return null;
  const rows: Element[] = [...table.querySelectorAll("tr")].filter((r) => r.closest("table") === table && texts(r).length > 0);
  const el = at.node.nodeType === Node.ELEMENT_NODE ? (at.node as Element) : at.node.parentElement;
  const cell = el?.closest("td, th") ?? null;
  const column = cell && cell.parentElement === row ? cellsOf(row).indexOf(cell) : 0;
  // Characters are counted in code points, so the cursor never lands inside a letter outside the BMP.
  let chars = 0;
  for (const t of cell ? texts(cell) : []) {
    if (t === at.node) { chars += [...t.data.slice(0, at.offset)].length; break; }
    chars += [...t.data].length;
  }
  const next = rows[rows.indexOf(row) + (direction === "forward" ? 1 : -1)];
  if (next) {
    const cells = cellsOf(next);
    const target = cells[Math.min(column, cells.length - 1)];
    let inCell = target ? texts(target) : [];
    if (inCell.length === 0) inCell = texts(next);
    for (const t of inCell) {
      const points = [...t.data];
      if (chars < points.length) return { node: t, offset: points.slice(0, chars).join("").length };
      chars -= points.length;
    }
    const last = inCell[inCell.length - 1];
    return { node: last, offset: lastCharOffset(last) };
  }
  const all = texts(rootOf()).filter((t) => !table.contains(t));
  if (direction === "forward") {
    const after = all.find((t) => (table.compareDocumentPosition(t) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0);
    return after ? { node: after, offset: 0 } : at;
  }
  const before = all.reverse().find((t) => (table.compareDocumentPosition(t) & Node.DOCUMENT_POSITION_PRECEDING) !== 0);
  return before ? { node: before, offset: lastCharOffset(before) } : at;
}

/** Linewise: both ends go out to the boundaries of the lines they are on, the focus's end last. */
function wholeLines(s: Alterable): void {
  const forward = !s.anchorNode || !s.focusNode ||
    order({ node: s.focusNode, offset: s.focusOffset }, { node: s.anchorNode, offset: s.anchorOffset }) >= 0;
  const dir: Direction = forward ? "forward" : "backward";
  const other: Direction = forward ? "backward" : "forward";
  const reverse = () => {
    if (s.anchorNode && s.focusNode) s.setBaseAndExtent(s.focusNode, s.focusOffset, s.anchorNode, s.anchorOffset);
  };
  s.modify("extend", dir, "lineboundary");
  reverse();
  s.modify("extend", other, "lineboundary");
  reverse();
}

/** The table row holding a place, if it is in one on the page. */
function rowOf(node: Node | null): Element | null {
  const el = node && (node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement);
  const row = el?.closest("tr") ?? null;
  return row && onPage(row) ? row : null;
}

/** Linewise in a table: a row is one line, as its source is. The engine's line boundary is a cell's (each cell is a
 *  block of its own), so an end in a row goes out to the row's first and last text. */
function wholeRows(s: Alterable): void {
  if (!s.anchorNode || !s.focusNode) return;
  const a: Place = { node: s.anchorNode, offset: s.anchorOffset };
  const f: Place = { node: s.focusNode, offset: s.focusOffset };
  const forward = order(f, a) >= 0;
  const [first, last] = forward ? [a, f] : [f, a];
  const out = (p: Place, start: boolean): Place => {
    const row = rowOf(p.node);
    const text = row ? texts(row) : [];
    if (text.length === 0) return p;
    const t = start ? text[0] : text[text.length - 1];
    return start ? { node: t, offset: 0 } : { node: t, offset: t.data.length };
  };
  const from = out(first, true);
  const to = out(last, false);
  if (forward) s.setBaseAndExtent(from.node, from.offset, to.node, to.offset);
  else s.setBaseAndExtent(to.node, to.offset, from.node, from.offset);
}

/** Draws vim's selection from the two ends: both characters and what lies between, the focus on the cursor's side
 *  (after its character going forward, before it going back); linewise, their whole lines (a table row whole). */
function draw(s: Alterable): void {
  if (!anchor || !cursor) return;
  const forward = order(cursor, anchor) >= 0;
  const a = forward ? anchor : step(s, anchor, "forward");
  const c = forward ? step(s, cursor, "forward") : cursor;
  s.setBaseAndExtent(a.node, a.offset, c.node, c.offset);
  if (kind === "line") {
    wholeLines(s);
    wholeRows(s);
  }
  paint(s);
}

/* The page's first and last text: `gg` and `G` stop there, and the selection never takes in what follows the
   page's text (the kit's HUD). */
function texts(root: Element): Text[] {
  const out: Text[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const text = n as Text;
    if (text.data.trim() !== "" && onPage(text) && !text.parentElement?.closest("[hidden], script, style, template")) {
      out.push(text);
    }
  }
  return out;
}

/** Where `v` starts without a page cursor: the first text in the view, as Vimium's caret does. */
function firstTextInView(root: Element): Place | null {
  const range = document.createRange();
  for (const text of texts(root)) {
    const offset = text.data.length - text.data.replace(/^\s+/, "").length;
    range.setStart(text, offset);
    range.setEnd(text, Math.min(offset + 1, text.data.length));
    const rects = range.getClientRects?.() ?? [];
    const rect = rects.length ? rects[0] : null;
    if (rect && rect.bottom > 0 && rect.top < window.innerHeight) return { node: text, offset };
  }
  return null;
}

/** A box of the cursor's character: the one before the focus going forward, the one after it going back. */
export function focusRect(s: Selection): DOMRect | null {
  const node = s.focusNode;
  if (!node) return null;
  const forward = !s.anchorNode || order({ node, offset: s.focusOffset }, { node: s.anchorNode, offset: s.anchorOffset }) >= 0;
  const range = document.createRange();
  if (node.nodeType === Node.TEXT_NODE) {
    const length = (node as Text).data.length;
    const at = forward ? Math.max(0, s.focusOffset - 1) : Math.min(s.focusOffset, Math.max(0, length - 1));
    range.setStart(node, at);
    range.setEnd(node, Math.min(at + 1, length));
  } else {
    const child = node.childNodes[Math.min(s.focusOffset, node.childNodes.length - 1)] ?? node;
    range.selectNodeContents(child);
  }
  const rects = range.getClientRects?.() ?? [];
  const rect = rects.length ? rects[rects.length - 1] : range.getBoundingClientRect?.();
  if (rect && (rect.width || rect.height)) return rect;
  const box = (node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement)?.getBoundingClientRect();
  return box && (box.width || box.height) ? box : null;
}

/** Keeps the cursor in view: the page places it (and its own cursor) when it can, else the window moves the least
 *  that keeps it an eighth of the view from either edge. */
function follow(s: Selection): void {
  const rect = focusRect(s);
  if (!rect) return;
  if (options.follow) {
    try {
      options.follow(rect);
    } catch (error) {
      console.warn("eitri keys: the page's visual follow failed", error);
    }
    return;
  }
  const hud = document.getElementById("eitri-keys-hud");
  const view = window.innerHeight - (hud && !hud.hidden ? hud.getBoundingClientRect().height : 0);
  const margin = Math.min(view * MARGIN_SHARE, currentSettings().scrollStep);
  const shift = rect.top < margin ? rect.top - margin : rect.bottom > view - margin ? rect.bottom - (view - margin) : 0;
  if (shift !== 0) scrollToAnimated(null, Math.max(0, windowDestination() + shift));
}

function say(name: KitMessage, values: { count?: number } = {}): void {
  showMessage(kitMessage(options.messages, name, values));
}

/** `v` (`char`) or `V` (`line`) from normal mode: a selection the reader made that is in view keeps its ends; else
 *  the mode starts at the page's cursor (`visual.start`), else at the first text in the view. False, with a message,
 *  when there is no text. */
export function startVisual(next: VisualKind): boolean {
  const s = selection();
  if (!s) {
    say("visual.noText");
    return false;
  }
  const mine = s.rangeCount > 0 && !s.isCollapsed && onPage(s.anchorNode) && onPage(s.focusNode) ? focusRect(s) : null;
  if (mine && mine.bottom > 0 && mine.top < window.innerHeight && s.anchorNode && s.focusNode) {
    const a = { node: s.anchorNode, offset: s.anchorOffset };
    const f = { node: s.focusNode, offset: s.focusOffset };
    // The reader's selection holds both end characters: the cursor's is the one before a focus that comes later.
    const forward = order(f, a) >= 0;
    anchor = forward ? a : step(s, a, "backward");
    cursor = forward ? step(s, f, "backward") : f;
  } else {
    let start: Range | null = null;
    try {
      start = options.start?.() ?? null;
    } catch (error) {
      console.warn("eitri keys: the page's visual start failed", error);
    }
    let at: Place | null = start && onPage(start.startContainer)
      ? { node: start.startContainer, offset: start.startOffset } : null;
    at ??= firstTextInView(rootOf());
    if (!at) {
      say("visual.noText");
      return false;
    }
    // A place at the end of the text has no character after it: the cursor stands on the last one.
    if (order(step(s, at, "forward"), at) <= 0) at = step(s, at, "backward");
    anchor = at;
    cursor = at;
  }
  kind = next;
  mark(kind);
  draw(s);
  follow(s);
  return true;
}

/** Leaves visual mode; the selection goes. */
export function endVisual(): void {
  if (!kind) return;
  kind = null;
  mark(null);
  unpaint();
  anchor = null;
  cursor = null;
  pending = "";
  resolver?.reset();
  document.getSelection()?.removeAllRanges();
  options.ended?.();
}

const strokes = (key: string) => parseNotation(key)?.map((k) => `${k.ctrl}${k.shift}${k.key}`).join(" ") ?? key;

function rows(): Row[] {
  const page = Object.keys(options.keys ?? {});
  const taken = new Set(page.map(strokes));
  return [
    ...page.map((key) => ({ action: `page:${key}`, keys: [key], context: "", desc: "" })),
    ...[...MOTIONS, ...COMMANDS].filter((key) => !taken.has(strokes(key)))
      .map((key) => ({ action: key, keys: [key], context: "", desc: "" })),
  ];
}

/** `y`: the selection as shown goes to the clipboard, and visual mode ends. The copy starts inside the key event:
 *  `execCommand("copy")` of the live selection first, then `navigator.clipboard.writeText` only when that failed. */
function yank(s: Selection): void {
  const text = s.toString();
  const count = [...text].length;
  let copied = false;
  try {
    copied = typeof document.execCommand === "function" && document.execCommand("copy") === true;
  } catch {
    copied = false;
  }
  const done = (ok: boolean) => say(ok ? (count === 1 ? "visual.yankedOne" : "visual.yanked") : "visual.copyFailed",
    { count });
  endVisual();
  if (copied) {
    done(true);
    return;
  }
  const clip = typeof navigator !== "undefined" ? navigator.clipboard : undefined;
  if (!clip || typeof clip.writeText !== "function") {
    done(false);
    return;
  }
  clip.writeText(text).then(() => done(true), () => done(false));
}

/** One key in visual mode; true when it was the mode's (the caller prevents its default). */
export function visualKey(stroke: KeyStroke, ev: KeyboardEvent, timeoutlen: () => number,
  scroll: (action: "scroll.down" | "scroll.up", ev: KeyboardEvent) => void): boolean {
  if (!kind) return false;
  const s = selection();
  if (!s || !standing() || !anchor || !cursor) {
    // The selection or its text went (the page replaced its text): so does the mode.
    endVisual();
    return stroke.key === "Escape";
  }
  resolver ??= createResolver(rows(), timeoutlen, () => performance.now(), { prefixWait: () => true });
  const result: Resolution = resolver.feed(stroke);
  pending = result.kind === "pending" ? result.prefix : "";
  if (result.kind === "pending" || result.kind === "cancelled") return true;
  if (result.kind !== "action") return !stroke.ctrl;
  const action = result.action;
  if (action.startsWith("page:")) {
    const run = options.keys?.[action.slice(5)];
    let took = false;
    try {
      took = run?.(kind) === true;
    } catch (error) {
      console.warn("eitri keys: the page's visual key failed", error);
    }
    // A page's own key acts on the selection and ends the mode, as `y` does.
    if (took) endVisual();
    return true;
  }
  try {
    act(s, action, ev, scroll);
  } catch (error) {
    // A place the page's text no longer has (an update changed it under the selection): the mode ends.
    console.warn("eitri keys: visual mode lost its text", error);
    endVisual();
  }
  return true;
}

/** Runs one of the mode's own keys. */
function act(s: Alterable, action: string, ev: KeyboardEvent,
  scroll: (action: "scroll.down" | "scroll.up", ev: KeyboardEvent) => void): void {
  if (!kind || !anchor || !cursor) return;
  switch (action) {
    case "<Esc>": endVisual(); return;
    case "y": yank(s); return;
    case "<C-e>": scroll("scroll.down", ev); return;
    case "<C-y>": scroll("scroll.up", ev); return;
    case "v": case "V": {
      const next: VisualKind = action === "v" ? "char" : "line";
      if (kind === next) { endVisual(); return; }
      kind = next;
      mark(kind);
      break;
    }
    case "o": [anchor, cursor] = [cursor, anchor]; break;
    default: {
      const next = moveCursor(s, cursor, action);
      // The cursor stays on the page's text: a move that would leave it (or found nothing) moves nothing.
      if (next && onPage(next.node)) cursor = next;
    }
  }
  draw(s);
  follow(s);
}
