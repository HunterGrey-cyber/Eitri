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
/* One character as the reader sees it, and as both engines' character steps pass it, is a grapheme cluster: a letter
   with its combining marks (a decomposed `é` is two code points), an emoji with its joiners, a flag. Without
   `Intl.Segmenter` (no engine Eitri ships lacks it) a cluster is a code point and the combining marks after it. The
   kit's library target predates the segmenter's types. */
type Cluster = { segment: string; index: number };
interface Segmenter { segment(text: string): Iterable<Cluster> & { containing(index: number): Cluster | undefined } }
const SegmenterCtor = typeof Intl === "undefined" ? undefined
  : (Intl as unknown as { Segmenter?: new (locale: undefined, o: { granularity: "grapheme" }) => Segmenter }).Segmenter;
const segmenter: Segmenter = SegmenterCtor ? new SegmenterCtor(undefined, { granularity: "grapheme" }) : {
  segment: (text) => {
    const all: Cluster[] = Array.from(text.matchAll(/\P{M}\p{M}*|\p{M}+/gu),
      (m) => ({ segment: m[0], index: m.index ?? 0 }));
    const containing = (i: number) => all.find((c) => i >= c.index && i < c.index + c.segment.length);
    return Object.assign(all, { containing });
  },
};

/** A string's characters, each a grapheme cluster. */
function clusters(text: string): string[] {
  return Array.from(segmenter.segment(text), (c) => c.segment);
}

/** The character (grapheme cluster) of a string that holds the code unit at `index`, read around it only. */
function clusterAt(text: string, index: number): Cluster | undefined {
  return segmenter.segment(text).containing(index);
}

/** The kit's own elements: a selection never reaches into them, and the page's text ends where they start. */
const KIT = "#eitri-keys-hud, #eitri-keys-sheet, #eitri-keys-find, #eitri-keys-hints";
/** Text nobody reads as the page's, besides what the page excludes from find: KaTeX draws its glyphs in it. */
const HIDDEN = "[aria-hidden='true']";
/** How far from the view's edges the cursor is kept on a page that does not place it itself. */
const MARGIN_SHARE = 1 / 8;

/** Visual mode's motions, vim's: fixed, as hint mode's letters are, so they are never declared. */
const MOTIONS = ["h", "l", "j", "k", "w", "b", "e", "0", "$", "{", "}", "gg", "G", "n", "N"];
const COMMANDS = ["o", "y", "v", "V", "<C-e>", "<C-y>", "<Esc>"];

let kind: VisualKind | null = null;
/** vim's two ends: the anchor's character (where the mode started, or the far end after `o`) and the cursor's,
 *  each as the place just before it. The selection drawn holds both characters and everything between them
 *  (linewise: their whole lines), as vim's does, whichever comes first. */
let anchor: Place | null = null;
let cursor: Place | null = null;
let options: VisualOptions & {
  root?: () => Element | null;
  /** The page's `find.exclude`: text under it is not the page's, here as in find. */
  exclude?: string;
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

/** What the page told `install` about visual mode, the find root the selection is kept inside, the text find leaves
 *  out, and the messages. */
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
  // Before the body exists (a script in the head) the whole document is the root.
  return root ?? document.body ?? document.documentElement;
}

function elementOf(node: Node): Element | null {
  return node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
}

/** Whether an element is text the page leaves out: `aria-hidden`, or under an element of the root that the page's
 *  `find.exclude` matches (KaTeX's MathML copy, which is clipped to a pixel and drawn by nobody). A selector the
 *  engine cannot parse leaves nothing out, as in find. */
function leftOut(el: Element, root: Element): boolean {
  if (el.closest(HIDDEN)) return true;
  if (!options.exclude) return false;
  try {
    const hit = el.closest(options.exclude);
    return !!hit && root.contains(hit);
  } catch {
    return false;
  }
}

/** Whether a place is the page's text: inside the root (the page's, unless the caller resolved it once), outside the
 *  kit's own elements, and not left out. */
function onPage(node: Node | null, root: Element = rootOf()): boolean {
  const el = node && elementOf(node);
  return !!el && root.contains(el) && !el.closest(KIT) && !leftOut(el, root);
}

/** Whether a place is inside the root in text the page leaves out: the walk passes such text as it passes an image,
 *  and a motion that lands in it moves out. */
function aside(node: Node, root: Element): boolean {
  const el = elementOf(node);
  return !!el && root.contains(el) && !el.closest(KIT) && leftOut(el, root);
}

/** The outermost element inside the root (the root included) that leaves a node's text out: `aria-hidden`, or one
 *  the page's `find.exclude` matches. The walk passes it whole, in one move, however much text it holds. */
function leftOutElement(node: Node, root: Element): Element | null {
  let out: Element | null = null;
  for (let el = elementOf(node); el && root.contains(el); el = el.parentElement) {
    if (el.matches(HIDDEN) || excluded(el)) out = el;
  }
  return out;
}

/** Whether the page's `find.exclude` matches an element itself; a selector the engine cannot parse matches nothing. */
function excluded(el: Element): boolean {
  if (!options.exclude) return false;
  try {
    return el.matches(options.exclude);
  } catch {
    return false;
  }
}

/** The place just past an element in a direction: after it going forward, before it going back. */
function past(el: Element, direction: Direction): Place | null {
  const parent = el.parentNode;
  if (!parent) return null;
  const index = Array.prototype.indexOf.call(parent.childNodes, el) as number;
  return { node: parent, offset: direction === "forward" ? index + 1 : index };
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
  return p.offset >= 0 && p.offset <= max;
}

function focusOf(s: Selection): Place | null {
  return s.focusNode ? { node: s.focusNode, offset: s.focusOffset } : null;
}

/* Movement, as Vimium's: the selection's own `modify`, with word moves done a character at a time so they behave
   the same on every platform. Each runs from a caret at a place and gives the place it reaches. */

function samePlace(a: Place | null, b: Place | null): boolean {
  return !!a && !!b && a.node === b.node && a.offset === b.offset;
}

/** Takes one character step; whether the focus moved (it does not at the start or the end of the text). */
function stepOn(s: Alterable, direction: Direction): boolean {
  const focus = focusOf(s);
  s.modify("extend", direction, "character");
  return !samePlace(focusOf(s), focus);
}

/** The text of the DOM between two places, split into the page's own and whether any of it is left out (inside the
 *  root). A character step spans one character, so this reads a few nodes. */
function textBetween(a: Place, b: Place, root: Element): { page: string; hidden: boolean } {
  const range = document.createRange();
  const [first, last] = order(a, b) <= 0 ? [a, b] : [b, a];
  range.setStart(first.node, first.offset);
  range.setEnd(last.node, last.offset);
  let page = "";
  let hidden = false;
  const walker = document.createTreeWalker(range.commonAncestorContainer, NodeFilter.SHOW_TEXT);
  walker.currentNode = range.startContainer;
  let node: Node | null = range.startContainer.nodeType === Node.TEXT_NODE ? range.startContainer : walker.nextNode();
  for (; node && range.comparePoint(node, 0) <= 0; node = walker.nextNode()) {
    if (!range.intersectsNode(node)) continue;
    const text = (node as Text).data;
    const part = text.slice(node === range.startContainer ? range.startOffset : 0,
      node === range.endContainer ? range.endOffset : text.length);
    if (part === "") continue;
    if (onPage(node, root)) page += part;
    else if (aside(node, root)) hidden = true;
  }
  return { page, hidden };
}

/** What a character step from `a` to `b` passed: the character the engine's selection text gained by it (a whole
 *  grapheme cluster, `clusters`: a letter with a combining accent is one, and is a word character as its letter is),
 *  or "" where it passed nothing of the page's text: an
 *  inline atom (an image, a checkbox: the engine steps over it with no text), only text the page leaves out, or
 *  nothing of the page's from or into such text (around KaTeX's MathML copy, which the engine lays out as a block,
 *  the selection's text gains line breaks). The gain can carry line breaks and spaces the engine writes at block and
 *  cell edges besides the character (Blink: `"a\n"` for the `a` before a formula that ends a paragraph), so the
 *  character is the one that is not white space, if there is one. A step between the page's places with no text in
 *  the DOM (a line break, a block's edge) keeps the engine's character. */
function passed(a: Place, b: Place, text: string, direction: Direction, root: Element): string {
  const points = clusters(text);
  const solid = points.filter((c) => !/\s/.test(c));
  const pick = solid.length ? solid : points;
  const ch = (direction === "forward" ? pick[0] : pick[pick.length - 1]) ?? "";
  if (ch === "") return "";
  const { page, hidden } = textBetween(a, b, root);
  if (page.includes(ch) || (/\s/.test(ch) && /\s/.test(page))) return ch;
  return hidden || aside(a.node, root) || aside(b.node, root) ? "" : ch;
}

/** A step of the walk: the place it started from, where it landed, the engine's selection text for it, and what it
 *  passed (`passed`: a character, or "" for an inline atom or left-out text). */
type Step = { from: Place; to: Place; text: string; ch: string; jumped?: boolean; last?: boolean };

/** One step on from a caret at `from` (back with `backward`), or null where the focus cannot move (the text's start
 *  or end). The selection holds only the step while its text is read, so a step costs the same however far the walk
 *  has come. Left-out text is passed whole: a step that passes none of the page's text and lands in it goes on past
 *  the outermost left-out element holding it (`jump`), as does a step from left-out text where the engine cannot
 *  move (Blink stops going back at a zero-width space in KaTeX's glyphs), or from the place right next to a left-out
 *  element where the engine cannot move (Blink cannot step back from the place between KaTeX's MathML copy and its
 *  glyphs). Where the engine cannot move from an element place next to an element of the page's, the step goes into
 *  it or past it (`into`). Moved with "" is not the text's end. */
function cross(s: Alterable, from: Place, direction: Direction, root: Element): Step | null {
  s.collapse(from.node, from.offset);
  const sign = direction === "forward" ? 1 : -1;
  stepOn(s, direction);
  const to = focusOf(s);
  // A focus that is not further on did not move: at the text's edge the engine (or a canonical place) can report
  // another node's place at the same point, or one behind.
  if (!to || order(to, from) * sign <= 0) {
    const inside = aside(from.node, root) ? from.node : beside(from, direction);
    if (!inside) return null;
    return aside(inside, root) ? jump(from, inside, direction, root) : into(from, inside, direction, root);
  }
  const text = s.toString();
  const ch = passed(from, to, text, direction, root);
  return (ch === "" && jump(from, to.node, direction, root)) || { from, to, text, ch };
}

/** The node right next to a place in a direction, at the same level: the child after (before) an element place, the
 *  sibling after (before) a text's end (start). */
function beside(p: Place, direction: Direction): Node | null {
  const forward = direction === "forward";
  if (p.node.nodeType !== Node.TEXT_NODE) return p.node.childNodes[forward ? p.offset : p.offset - 1] ?? null;
  const edge = forward ? p.offset === (p.node as Text).data.length : p.offset === 0;
  return edge ? (forward ? p.node.nextSibling : p.node.previousSibling) : null;
}

/** A step from `from` past the outermost left-out element holding `inside`, which passes an atom; null where
 *  `inside` is not left out or the place past the element is not further on than `from`. Where none of the page's
 *  text lies beyond the element (a document that opens with a diagram has nothing before it to go back to) the step
 *  is the `last`: a word walk going back does not take it. */
function jump(from: Place, inside: Node, direction: Direction, root: Element): Step | null {
  const el = aside(inside, root) ? leftOutElement(inside, root) : null;
  const to = el && past(el, direction);
  if (!to || order(to, from) * (direction === "forward" ? 1 : -1) <= 0) return null;
  return { from, to, text: "", ch: "", jumped: true, last: !beyond(el, direction, root) };
}

/** A step from an element place into the page's element right next to it, where the engine does not move: both
 *  engines stop going back at the place just after an `<hr>` or a table that is a child of the root. The step goes to
 *  the nearest place of the page's text inside the element (its last text's end going back, its first text's start
 *  going forward), or past the element where it holds none (the `last` step where none lies beyond it either); it
 *  passes an atom. Null from a text, or beside an element that is not the page's. */
function into(from: Place, el: Node, direction: Direction, root: Element): Step | null {
  if (from.node.nodeType === Node.TEXT_NODE || el.nodeType !== Node.ELEMENT_NODE || !onPage(el, root)) return null;
  const inside = texts(el as Element, root);
  const text = direction === "forward" ? inside[0] : inside[inside.length - 1];
  const to = text ? { node: text, offset: direction === "forward" ? 0 : text.data.length }
    : past(el as Element, direction);
  return to && { from, to, text: "", ch: "", last: !text && !beyond(el as Element, direction, root) };
}

/** Walks on from the focus while the character ahead is a word character (`word`) or is not one, and leaves a caret
 *  where it stopped; an inline atom, or text the page leaves out, is not one, so the walk passes it as it passes a
 *  space. */
function over(s: Alterable, direction: Direction, word: boolean, root: Element): void {
  let at = focusOf(s);
  if (!at) return;
  let jumped = false;
  // Going back, the walk does not take a step into left-out text with none of the page's before it: the page's text
  // starts after it, so the walk stays on its first character, as at the start of any page. Going forward such a step
  // leads off the page's text, as past the end of any page.
  const takes = (step: Step) => !step.last || direction === "forward";
  for (let next = cross(s, at, direction, root); next && takes(next); next = cross(s, at, direction, root)) {
    if (WORD.test(next.ch) !== word) {
      // Stopped going forward right past left-out text, or at any place that is not in a text (after an `<hr>`, a
      // table, an image): such a place can be in another block than the character after it, so the walk stands
      // just before that character instead.
      if (direction === "forward" && (jumped || at.node.nodeType !== Node.TEXT_NODE)) at = charBefore(next) ?? at;
      break;
    }
    at = next.to;
    jumped = !!next.jumped;
  }
  s.collapse(at.node, at.offset);
}

/** The place just before the character a forward step passed, where it lies at the end of the step in a text. */
function charBefore(step: Step): Place | null {
  const { to, ch } = step;
  if (ch === "" || to.node.nodeType !== Node.TEXT_NODE || to.offset < ch.length) return null;
  const start = to.offset - ch.length;
  return (to.node as Text).data.slice(start, to.offset) === ch ? { node: to.node, offset: start } : null;
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
function moveCursor(s: Alterable, at: Place, motion: string, root: Element): Place | null {
  switch (motion) {
    case "h": return step(s, at, "backward");
    case "l": return step(s, at, "forward");
    case "j": return rowMove(at, "forward", root) ?? from(s, at, (sel) => sel.modify("extend", "forward", "line"));
    case "k": return rowMove(at, "backward", root) ?? from(s, at, (sel) => sel.modify("extend", "backward", "line"));
    case "b":
      // Back over what is not a word, then over the word's characters to its first: a character at a time with the
      // same WORD as `w` and `e`, not the engine's word granularity (which splits a Chinese run in the middle).
      return from(s, at, (sel) => {
        over(sel, "backward", false, root);
        over(sel, "backward", true, root);
      });
    case "0": return from(s, at, (sel) => sel.modify("extend", "backward", "lineboundary"));
    case "w":
      // To the end of this word, then over what follows it up to the next word.
      return from(s, at, (sel) => {
        over(sel, "forward", true, root);
        over(sel, "forward", false, root);
      });
    case "e": {
      // Past the cursor's own character, over what is not a word, to the end of the next word's last character.
      const end = from(s, at, (sel) => {
        stepOn(sel, "forward");
        over(sel, "forward", false, root);
        over(sel, "forward", true, root);
      });
      return order(end, at) > 0 ? step(s, end, "backward") : at;
    }
    case "$": {
      const end = from(s, at, (sel) => sel.modify("extend", "forward", "lineboundary"));
      return order(end, at) > 0 ? step(s, end, "backward") : at;
    }
    case "}": return blockMove(at, "forward", root);
    case "{": return blockMove(at, "backward", root);
    case "gg": case "G": {
      const all = texts(root, root);
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

/** The motions `normalMotion` answers, and visual mode places its cursor for the same way (`lineMotion`): the cursor
 *  moves of a line, as Normal mode keeps its cursor on one character. */
const NORMAL_MOTIONS = new Set(["h", "l", "w", "b", "e", "0", "$"]);

/** What `normalMotion` borrows of the document's selection: its two ends, and the Range it held. */
type Held = { a: Node; ao: number; b: Node; bo: number; range: Range } | null;

function hold(s: Selection): Held {
  if (s.rangeCount === 0 || !s.anchorNode || !s.focusNode) return null;
  return { a: s.anchorNode, ao: s.anchorOffset, b: s.focusNode, bo: s.focusOffset, range: s.getRangeAt(0) };
}

/** Puts the borrowed selection back, never throwing: a forward one as the very Range it was (a plugin holding
 *  `getRangeAt(0)` still holds the live one), a backward one by its ends; where the saved ends are gone, no selection
 *  rather than the walker's. */
function putBack(s: Selection, held: Held): void {
  try {
    s.removeAllRanges();
    if (!held) return;
    const r = held.range;
    if (r.startContainer === held.a && r.startOffset === held.ao && r.endContainer === held.b && r.endOffset === held.bo) {
      s.addRange(r);
    } else {
      s.setBaseAndExtent(held.a, held.ao, held.b, held.bo);
    }
  } catch {
    try {
      s.removeAllRanges();
    } catch {
      // Nothing is left to undo.
    }
  }
}

/** Whether a field or an editable element has the focus: the document's selection then belongs to it (it reads as the
 *  element's host, and putting it back by its ends would drop the field's own caret). */
function editing(): boolean {
  return !!document.activeElement?.closest("input, textarea, select, [contenteditable]:not([contenteditable='false'])");
}

/** The line a place is on, for a place in left-out text too: the layout block of its nearest element that is the
 *  page's (KaTeX lays its hidden copy out as blocks and table cells of its own, inside an inline span). */
function lineOf(node: Node, root: Element): Element | null {
  let el = elementOf(node);
  while (el && root.contains(el) && !onPage(el, root)) el = el.parentElement;
  return el ? layoutBlock(el) : null;
}

/** The place before the one character a step passed, when it passed exactly one (a grapheme cluster) inside a text on
 *  `line`: not a line break (the engine's selection text has one where a line ends) and not into another block. */
function characterOf(step: Step, line: Element | null): Place | null {
  const { to, text } = step;
  if (to.node.nodeType !== Node.TEXT_NODE || clusters(text).length !== 1) return null;
  const start = to.offset - text.length;
  if (start < 0 || (to.node as Text).data.slice(start, to.offset) !== text) return null;
  return layoutBlock(to.node) === line ? { node: to.node, offset: start } : null;
}

/** The place just before the character the cursor is on, for a place the engine reports at a boundary: the end of a
 *  text (or an element place) in front of an inline element's text is reported as the end of the one before, not the
 *  start of the next. The engine's own next step says which character follows: past any inline atoms and left-out
 *  text, the one it passes, if on the same line. A place of the page's with nothing after it on its line (its end)
 *  stays as it is; a place in left-out text with nothing after it is null. */
function beforeCharacter(s: Alterable, at: Place, root: Element): Place | null {
  const text = at.node.nodeType === Node.TEXT_NODE;
  if (text && onPage(at.node, root) && at.offset < (at.node as Text).data.length) return at;
  // An element place in a block with none of the page's text (before a diagram's SVG) is on no line of the page's,
  // as a place in left-out text is.
  const line = lineOf(at.node, root);
  const end = onPage(at.node, root) && (text || (!!line && texts(line, root, 1).length > 0)) ? at : null;
  // A step out of the root passes no atom (`passed`): the page's text has ended there.
  for (let step = cross(s, at, "forward", root); step && !step.last; step = cross(s, step.to, "forward", root)) {
    if (step.ch !== "") return (onPage(step.to.node, root) && characterOf(step, line)) || end;
  }
  return end;
}

/** Whether an element place stands between blocks (after an `<hr>` or a table, between two paragraphs): a place in a
 *  block of the page's with nothing but blocks and white space on either side of it. */
function betweenBlocks(p: Place, root: Element): boolean {
  if (p.node.nodeType !== Node.ELEMENT_NODE || !onPage(p.node, root) || layoutBlock(p.node) !== p.node) return false;
  const kids = p.node.childNodes;
  const flat = (n: Node | undefined) => !n || (n.nodeType === Node.TEXT_NODE ? (n as Text).data.trim() === "" :
    n.nodeType === Node.ELEMENT_NODE && layoutBlock(n) === n);
  return flat(kids[p.offset - 1]) && flat(kids[p.offset]);
}

/** The motions whose last step goes back onto a character (`e` and `$` step back from the end they reach). */
const ENDS_BACK = new Set(["h", "e", "$"]);

/** Whether `settle` goes on back from `p`: a place that stands before an inline atom or left-out text is not the
 *  character the motion stepped back onto, nor is a place in left-out text unless the engine reports it for the
 *  page's character after it (WebKit gives the place before the space after a formula as one inside the formula). */
function goesBack(s: Alterable, p: Place, root: Element): boolean {
  const ahead = cross(s, p, "forward", root);
  if (!ahead) return aside(p.node, root);
  if (ahead.ch === "") return true;
  return aside(p.node, root) && !(onPage(ahead.to.node, root) && characterOf(ahead, lineOf(p.node, root)));
}

/** The answer for a place a motion reached: when the motion ends going back, a place in left-out text or before an
 *  inline atom goes back past them (`h` from the text after an image or a formula reaches the character before it);
 *  then it is the place before the character the cursor is on. A place still in left-out text (the root's text
 *  starts with it, or the motion does not end going back) goes on to the next character on its line, and where there
 *  is none, back as `h` goes (`l` from the last character before a formula that ends the line stays on it); null
 *  where neither finds one. */
function settle(s: Alterable, at: Place, root: Element, back: boolean): Place | null {
  let p = at;
  while (back && goesBack(s, p, root)) {
    const q = cross(s, p, "backward", root)?.to;
    if (!q || !(onPage(q.node, root) || aside(q.node, root))) break;
    p = q;
  }
  if (back) p = lineEndBefore(p, root);
  return beforeCharacter(s, p, root) ?? (back ? null : settle(s, at, root, true));
}

/** Going back from a block's first character the engine stops between blocks after an `<hr>` or a table: that place
 *  is the end of the line before, the end of the page's last text before it, as the end of the text before is after
 *  a paragraph (with no text before it, the page's text starts after it: `h` there stays on its first character).
 *  Any other place is itself. */
function lineEndBefore(p: Place, root: Element): Place {
  if (!betweenBlocks(p, root)) return p;
  return textBeside(p, "backward", root) ?? textBeside(p, "forward", root) ?? p;
}

/** The nearest of the page's texts on one side of an element place: the end of the last one before it going back,
 *  the start of the first one after it going forward. */
function textBeside(p: Place, direction: Direction, root: Element): Place | null {
  const forward = direction === "forward";
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const child: Node | undefined = p.node.childNodes[forward ? p.offset : p.offset - 1];
  // The walk starts at the side's nearest node: the child after the place, the last node inside the child before it,
  // or (with no child on that side) the last node inside the place's own element going forward, that element going
  // back, so it reads only what lies on that side.
  let start: Node = child ?? p.node;
  if (child ? !forward : forward) while (start.lastChild) start = start.lastChild;
  walker.currentNode = start;
  const next = () => (forward ? walker.nextNode() : walker.previousNode()) as Text | null;
  let t = child && start.nodeType === Node.TEXT_NODE ? (start as Text) : next();
  while (t && !pageText(t, root)) t = next();
  return t && { node: t, offset: forward ? 0 : t.data.length };
}

/** Where one of `h l w b e 0 $` takes a cursor standing just before a character at `at`, by the walker visual mode
 *  moves its cursor with, for a page that keeps a cursor of its own in Normal mode (kit 6). The answer is the place
 *  just before the character the cursor is then on: for `e` and `$` that is their last character (the walker's cursor
 *  is already the place before its character, as visual mode keeps it, so nothing is converted), and a place the
 *  engine reports at the end of a text with a character after it on the line is that character's. An inline atom (an
 *  image, a checkbox) is no character, nor is text the page leaves out (`aria-hidden`, its `find.exclude`: KaTeX's
 *  glyphs and its hidden MathML copy): `h`, `e` and `$` go back past it, the others on, and a step that lands inside
 *  such text moves out of it. Where nothing follows on the line, the answer is its end as the engine gives it (a
 *  text's end or an element place); the engine can also stand it before a character that draws nothing (a collapsed
 *  newline or space), so a page checks the character's box. The document's selection is borrowed for the walk and put
 *  back as it was (anchor and focus, so a backward selection stays backward; none stays none); the page's root is
 *  resolved once, before. Null for a start outside the page's text, a place its text no longer has, a key that is not
 *  one of the seven, an engine without `Selection.modify`, a focus in a field, and a result outside the root (or in
 *  left-out text with no character of the page's on its line either way). */
export function normalMotion(from: Place, motion: string): Place | null {
  const s = selection();
  if (!s || !NORMAL_MOTIONS.has(motion) || editing()) return null;
  const root = rootOf();
  if (!from.node.isConnected || !onPage(from.node, root) || !exists(from)) return null;
  const held = hold(s);
  try {
    return lineMotion(s, from, motion, root);
  } catch {
    return null;
  } finally {
    putBack(s, held);
  }
}

/** Where one of `h l w b e 0 $` takes a cursor standing just before a character at `at`, in `normalMotion`'s shape, for
 *  visual mode's cursor and a page's Normal-mode one alike: the walker's place, settled (`settle`) onto the character
 *  the cursor is then on, so the selection drawn, `follow`'s cursor and `motion`'s answer agree past an inline atom or
 *  left-out text. Null where the walker leaves the page's text. */
function lineMotion(s: Alterable, at: Place, motion: string, root: Element): Place | null {
  const next = moveCursor(s, at, motion, root);
  if (!next || !(onPage(next.node, root) || aside(next.node, root))) return null;
  const placed = settle(s, next, root, ENDS_BACK.has(motion));
  return placed && onPage(placed.node, root) ? placed : null;
}

/** The offset of the last character (a whole grapheme cluster) of a string; 0 for an empty one. */
function lastCharOffset(data: string): number {
  return data === "" ? 0 : clusterAt(data, data.length - 1)?.index ?? data.length - 1;
}

/** The nearest element laid out as a block (a paragraph, a list item's own text, a heading, a code block, a table
 *  cell) holding a place, itself included: not inline. */
function layoutBlock(node: Node): Element | null {
  for (let el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement; el; el = el.parentElement) {
    const display = getComputedStyle(el).display;
    if (display && !display.startsWith("inline") && display !== "contents") return el;
  }
  return null;
}

/** The block a place belongs to for `{` and `}`: its table row, else its layout block. */
function blockOf(node: Node, known: Map<Node, Element | null>): Element | null {
  const had = known.get(node);
  if (had !== undefined) return had;
  const el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
  const block = el?.closest("tr") ?? layoutBlock(node);
  known.set(node, block);
  return block;
}

/** `}` and `{`, vim's paragraph motions over the page's blocks: `}` to the last character of the cursor's block, or
 *  from there to the last of the next block; `{` to the first character of the cursor's block, or from there to the
 *  first of the block before. So a selection grows a whole block at a time, as `v}` and `V}` grow by paragraphs.
 *  White space at a block's edges is passed over. */
function blockMove(at: Place, direction: Direction, root: Element): Place | null {
  const all = texts(root, root);
  if (all.length === 0) return null;
  const known = new Map<Node, Element | null>();
  let i = all.indexOf(at.node as Text);
  if (i < 0) {
    // A place between texts: the text it stands before (forward) or after (backward).
    i = direction === "forward" ? all.findIndex((t) => order({ node: t, offset: 0 }, at) >= 0)
      : all.length - 1 - [...all].reverse().findIndex((t) => order({ node: t, offset: 0 }, at) < 0);
    if (i < 0 || i >= all.length) return null;
  }
  // The run of texts around `k` in one block.
  const run = (k: number): [number, number] => {
    const block = blockOf(all[k], known);
    let a = k;
    let b = k;
    while (a > 0 && blockOf(all[a - 1], known) === block) a--;
    while (b + 1 < all.length && blockOf(all[b + 1], known) === block) b++;
    return [a, b];
  };
  const first = (t: Text): Place => ({ node: t, offset: t.data.length - t.data.replace(/^\s+/, "").length });
  const last = (t: Text): Place => ({ node: t, offset: lastCharOffset(t.data.replace(/\s+$/, "")) });
  const [a, b] = run(i);
  if (direction === "forward") {
    const end = last(all[b]);
    if (order(end, at) > 0) return end;
    return b + 1 < all.length ? last(all[run(b + 1)[1]]) : null;
  }
  const start = first(all[a]);
  if (order(start, at) < 0) return start;
  return a > 0 ? first(all[run(a - 1)[0]]) : null;
}

/** The cells of a table row, in order. */
function cellsOf(row: Element): Element[] {
  return [...row.children].filter((c) => c.matches("td, th"));
}

/** `j` and `k` from a table row. The engine's line move goes from a cell to the next cell of the same row (each cell
 *  is a block of its own), so the cursor goes to the row below (above) instead: the cell in the same column, at the
 *  same character or the cell's last; past the table's last (first) row, to the first text after the table (the
 *  last character before it). Null outside a table. */
function rowMove(at: Place, direction: Direction, root: Element): Place | null {
  const row = rowOf(at.node, root);
  const table = row?.closest("table");
  if (!row || !table) return null;
  const rows: Element[] = [...table.querySelectorAll("tr")]
    .filter((r) => r.closest("table") === table && texts(r, root).length > 0);
  const el = at.node.nodeType === Node.ELEMENT_NODE ? (at.node as Element) : at.node.parentElement;
  const cell = el?.closest("td, th") ?? null;
  const column = cell && cell.parentElement === row ? cellsOf(row).indexOf(cell) : 0;
  // Characters are counted in grapheme clusters, so the cursor never lands inside a letter outside the BMP or between
  // a letter and its combining accent.
  let chars = 0;
  for (const t of cell ? texts(cell, root) : []) {
    if (t === at.node) { chars += clusters(t.data.slice(0, at.offset)).length; break; }
    chars += clusters(t.data).length;
  }
  const next = rows[rows.indexOf(row) + (direction === "forward" ? 1 : -1)];
  if (next) {
    const cells = cellsOf(next);
    const target = cells[Math.min(column, cells.length - 1)];
    let inCell = target ? texts(target, root) : [];
    if (inCell.length === 0) inCell = texts(next, root);
    for (const t of inCell) {
      const inText = clusters(t.data);
      if (chars < inText.length) return { node: t, offset: inText.slice(0, chars).join("").length };
      chars -= inText.length;
    }
    const last = inCell[inCell.length - 1];
    return { node: last, offset: lastCharOffset(last.data) };
  }
  return beyond(table, direction, root) ?? at;
}

/** Past an element a line move went into or across: the start of the page's first text after it (forward), or the
 *  last character of its last text before it (back); null where there is none. */
function beyond(el: Element, direction: Direction, root: Element): Place | null {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  // The walk starts at the element's last node going forward, so it reads only what lies beyond the element.
  let start: Node = el;
  if (direction === "forward") while (start.lastChild) start = start.lastChild;
  walker.currentNode = start;
  const next = () => (direction === "forward" ? walker.nextNode() : walker.previousNode()) as Text | null;
  for (let t = next(); t; t = next()) {
    if (pageText(t, root)) return { node: t, offset: direction === "forward" ? 0 : lastCharOffset(t.data) };
  }
  return null;
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
function rowOf(node: Node | null, root: Element): Element | null {
  const el = node && (node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement);
  const row = el?.closest("tr") ?? null;
  return row && onPage(row, root) ? row : null;
}

/** Linewise in a table: a row is one line, as its source is. The engine's line boundary is a cell's (each cell is a
 *  block of its own), so an end in a row goes out to the row's first and last text. */
function wholeRows(s: Alterable, root: Element): void {
  if (!s.anchorNode || !s.focusNode) return;
  const a: Place = { node: s.anchorNode, offset: s.anchorOffset };
  const f: Place = { node: s.focusNode, offset: s.focusOffset };
  const forward = order(f, a) >= 0;
  const [first, last] = forward ? [a, f] : [f, a];
  const out = (p: Place, start: boolean): Place => {
    const row = rowOf(p.node, root);
    const text = row ? texts(row, root) : [];
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
  const root = rootOf();
  const a = forward ? anchor : pastCharacter(s, anchor, root);
  const c = forward ? pastCharacter(s, cursor, root) : cursor;
  s.setBaseAndExtent(a.node, a.offset, c.node, c.offset);
  if (kind === "line") {
    wholeLines(s);
    wholeRows(s, root);
  }
  paint(s);
}

/** The place just past the character after `at`, where the selection drawn ends. A step that lands in left-out text
 *  (the engine reports the end of a character before a formula inside it, and steps into a diagram from the end of
 *  the text before it) ends at the character's end in its own text instead, or at `at` where it is a text's end. */
function pastCharacter(s: Alterable, at: Place, root: Element): Place {
  const next = step(s, at, "forward");
  if (!aside(next.node, root)) return next;
  if (at.node.nodeType !== Node.TEXT_NODE) return at;
  const char = clusterAt((at.node as Text).data, at.offset);
  return char ? { node: at.node, offset: char.index + char.segment.length } : at;
}

/* The page's texts under `scope` (only the first `most`), for the root the caller resolved (a walk borrowing the
   selection never resolves it again: a page's root can follow the selection): `gg` and `G` stop at the first and last,
   and the selection never takes in what follows the page's text (the kit's HUD). */
function texts(scope: Element, root: Element, most = Infinity): Text[] {
  const out: Text[] = [];
  const walker = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n && out.length < most; n = walker.nextNode()) {
    if (pageText(n as Text, root)) out.push(n as Text);
  }
  return out;
}

/** Whether a text is one `texts` lists: not white space only, the page's, and not in hidden or non-content markup or
 *  a closed `details`' folded content (none of which is drawn, so a line move past a block never lands in it). */
function pageText(text: Text, root: Element): boolean {
  return text.data.trim() !== "" && onPage(text, root) &&
    !text.parentElement?.closest("[hidden], script, style, template") && !folded(text);
}

/** Whether a node is in a closed `details` and not in that details' own summary, which stays drawn. */
function folded(node: Node): boolean {
  const closed = (el: Element | null | undefined) => el?.closest("details:not([open])");
  for (let d = closed(elementOf(node)); d; d = closed(d.parentElement)) {
    const summary = [...d.children].find((c) => c.matches("summary"));
    if (!summary?.contains(node)) return true;
  }
  return false;
}

/** Where `v` starts without a page cursor: the first text in the view, as Vimium's caret does. */
function firstTextInView(root: Element): Place | null {
  const range = document.createRange();
  for (const text of texts(root, root)) {
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

/** Draws the selection and keeps the cursor in view. The page's `follow` also gets the cursor's place in `motion`'s
 *  shape (the place just before its character, or the line's end as the engine gives it), worked out before the
 *  selection is drawn: under `V` the selection's focus is the end of the line (or of a table row), not the cursor. */
function show(s: Alterable): void {
  const at = cursor && (beforeCharacter(s, cursor, rootOf()) ?? cursor);
  draw(s);
  follow(s, at);
}

/** Keeps the cursor in view: the page places it (and its own cursor) when it can, else the window moves the least
 *  that keeps it an eighth of the view from either edge. */
function follow(s: Selection, at: Place | null): void {
  const rect = focusRect(s);
  if (!rect) return;
  if (options.follow) {
    try {
      options.follow(rect, at ? { node: at.node, offset: at.offset } : undefined);
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
  show(s);
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

/** The line and block motions: a move of theirs into left-out text goes on past it in the motion's direction. */
const VERTICAL = new Map<string, Direction>([["j", "forward"], ["}", "forward"], ["k", "backward"], ["{", "backward"]]);

/** Where one of visual mode's other moves (not `lineMotion`'s) from `start` that landed in left-out text goes: out of
 *  it onward as the Normal-mode cursor does (`settle`), except a line or block move onto a line with none of the page's text (a diagram, a display formula),
 *  which goes on past the left-out element to the page's text beyond it, as past a table's last row. A line move
 *  onto an inline formula stays on that formula's line unless settling there would not take the cursor on in the
 *  move's direction (Blink's line move from the text before a formula that ends its line lands in that formula's
 *  MathML copy). */
function outOf(s: Alterable, start: Place, at: Place, action: string, root: Element): Place | null {
  const direction = VERTICAL.get(action);
  const el = direction ? leftOutElement(at.node, root) : null;
  if (!direction || !el) return settle(s, at, root, false);
  const block = blockHolding(el);
  const placed = block && texts(block, root, 1).length > 0 ? settle(s, at, root, false) : null;
  const onward = placed && order(placed, start) * (direction === "forward" ? 1 : -1) > 0;
  return onward ? placed : beyond(el, direction, root);
}

/** The block whose lines a left-out element is laid out on: itself where it is a block in the flow (an
 *  `aria-hidden` block, a display formula's glyphs), else its parent's (an inline formula's glyphs, or KaTeX's MathML
 *  copy, which is absolutely positioned on its formula's line). */
function blockHolding(el: Element): Element | null {
  const position = getComputedStyle(el).position;
  if (layoutBlock(el) === el && position !== "absolute" && position !== "fixed") return el;
  return el.parentElement && layoutBlock(el.parentElement);
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
      const root = rootOf();
      let next: Place | null;
      if (NORMAL_MOTIONS.has(action)) {
        next = lineMotion(s, cursor, action, root);
      } else {
        next = moveCursor(s, cursor, action, root);
        if (next && aside(next.node, root)) next = outOf(s, cursor, next, action, root);
      }
      // The cursor stays on the page's text: a move that would leave it (or found nothing) moves nothing.
      if (next && onPage(next.node, root)) cursor = next;
    }
  }
  show(s);
}
