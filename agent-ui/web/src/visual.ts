/**
 * BROWSE's VISUAL mode copies text using the DOM `Selection.modify` API
 * except for `w`/`e`/`b`, which walk DOM text using word rules implemented by `wordForward`.
 * `App.tsx` owns `PanelMode`'s `"visual"`/`"vline"` modes, the `VisualModel` this module reads
 * and returns, and effects such as flashing, clipboard writes and ending the mode.
 *
 * jsdom has no `Selection.modify`, so live-selection functions accept `SelectionLike`.
 * A real `Selection` satisfies it structurally, and tests can supply a fake. Plain DOM
 * operations over `Node`, `Element`, `Range` and `TreeWalker` are exercised in jsdom too.
 */
import type { VisualMotion } from "./keymap";
import { conversationRows, rowOf } from "./nav";

/** A DOM boundary point before `offset` inside `node`, as used by `Selection.collapse`/`setBaseAndExtent`.
 *  Plain data: the live selection is rebuilt from it on each key instead of retaining a `Range`. */
export type Caret = { node: Node; offset: number };

/** CARET/VISUAL/V-LINE state: selection ends, selection kind and goal column.
 *  The ends are equal in CARET and move together. `kind` is `"caret"` for the block caret,
 *  `"char"` for VISUAL or `"line"` for V-LINE. `goalX` keeps the x coordinate of the first
 *  vertical move until a horizontal move resets it to null. */
export type VisualModel = { anchor: Caret; cursor: Caret; kind: "caret" | "char" | "line"; goalX: number | null };

/** The subset of `Selection` this module needs. A real `Selection` satisfies this structurally;
 *  tests fake it, since jsdom has no `modify`. */
export interface SelectionLike {
  readonly anchorNode: Node | null;
  readonly anchorOffset: number;
  readonly focusNode: Node | null;
  readonly focusOffset: number;
  readonly rangeCount: number;
  getRangeAt(index: number): Range;
  collapse(node: Node, offset: number): void;
  setBaseAndExtent(anchorNode: Node, anchorOffset: number, focusNode: Node, focusOffset: number): void;
  modify(alter: "move" | "extend", direction: "forward" | "backward", granularity: "character" | "word" | "line" | "paragraphboundary"): void;
  toString(): string;
}

/** Chrome excluded from caret placement and copying. Every selector also has `user-select: none`
 *  in `index.css`; `indexCss.test.ts` keeps the two lists aligned.
 *  `.row-sign` and `[aria-hidden="true"]` cover the diff gutter; `.fold-marker` is the run row's `▸`. */
export const VISUAL_CHROME: string[] = [".row-sign", '[aria-hidden="true"]', "button", "input", "textarea", "[data-nav-action]", ".fold-marker"];

const CHROME_SELECTOR = VISUAL_CHROME.join(", ");

function elementOf(node: Node): Element | null {
  return node instanceof Element ? node : node.parentElement;
}

/** Whether `node` is inside a `VISUAL_CHROME` element contained by `root`. */
export function isChromeNode(node: Node, root: Node): boolean {
  const el = elementOf(node);
  if (el === null) return false;
  const chrome = el.closest(CHROME_SELECTOR);
  return chrome !== null && root.contains(chrome);
}

/** Whether `node` itself keeps everything inside it off the page: the `hidden` attribute, or a
 *  child of a closed `<details>` other than its summary -- both survive the markdown sanitizer, so a
 *  reply can carry text that is in the DOM yet never drawn. */
function hidesItsContent(node: Node): boolean {
  if (node instanceof Element && node.hasAttribute("hidden")) return true;
  const parent = node.parentElement;
  if (parent === null || parent.tagName !== "DETAILS" || parent.hasAttribute("open")) return false;
  const summary = Array.from(parent.children).find((child) => child.tagName === "SUMMARY");
  return node !== summary;
}

/** Whether `node` is inside (or is) something not rendered, up to `root`. A caret never rests on,
 *  walks through, or copies from such text: `selectableTextWalker` and the word walk both skip it. */
export function isUnrenderedNode(node: Node, root: Node): boolean {
  for (let at: Node | null = node; at !== null && at !== root; at = at.parentNode) {
    if (hidesItsContent(at)) return true;
  }
  return false;
}

function caretsEqual(a: Caret, b: Caret): boolean {
  return a.node === b.node && a.offset === b.offset;
}

/** -1/0/1: whether `a` is before, at, or after `b` in document order (`Range.compareBoundaryPoints`,
 *  which jsdom implements the same as a real engine -- no layout needed, just tree order). */
export function compareCarets(a: Caret, b: Caret): number {
  if (caretsEqual(a, b)) return 0;
  const ra = document.createRange();
  ra.setStart(a.node, a.offset);
  const rb = document.createRange();
  rb.setStart(b.node, b.offset);
  return ra.compareBoundaryPoints(Range.START_TO_START, rb);
}

/* `w`/`e`/`b` follow vim's word rules (`:help word`), computed over the DOM text with
 * `wordClassOf` rather than `Selection.modify`'s word granularity. WebKit's word boundaries are
 * not vim's: underscores, punctuation and syntax-highlight spans can make composed
 * forward/backward word steps return to their starting position.
 * A word is a maximal run of one vim character class (nvim's `charclass()` table): keyword
 * characters, other non-blank characters, emoji, ideographs, kana, hangul and a few more.
 * Characters are grapheme clusters (`graphemeStarts`), so movement never lands on combining
 * marks or inside emoji sequences, even when highlighting splits a cluster across text nodes.
 * Whitespace and block boundaries separate words, as do `<br>`/`<hr>`/`<img>`, empty blocks and
 * intervening `VISUAL_CHROME` text. `wordTokens` treats these boundaries as line breaks.
 * Highlight spans do not split words, and `isUnrenderedNode` excludes hidden elements and closed
 * `<details>` bodies. Unlike vim, an empty line is not a word, and a `w`/`b` that would end on a
 * blank lands on the nearest non-blank character instead, since carets never rest on blanks here. */

/** vim's class of one character (`charclass()`): 0 blank, 1 punctuation, 2 keyword, 3 emoji, and
 *  vim's own larger numbers for CJK ideographs, hiragana, katakana, hangul syllables, superscripts,
 *  subscripts and braille -- so `中文x` is two words and `カーキ`/`ア・イ` one each, as in vim. */
type WordClass = number;

/** nvim's own `charclass()` for every code point from U+0100 up whose class is not 2 (keyword),
 *  as `hex[-hex]:class`, ascending. Generated with nvim v0.12.5 (`nvim --headless --clean`,
 *  `vim.fn.charclass(vim.fn.nr2char(c))` for c = 0x100..0x10ffff, consecutive equal classes merged,
 *  class-2 ranges dropped) rather than transcribed: it is `mbyte.c`'s `utf_class` table together
 *  with its emoji table, which no Unicode property reproduces (U+30FC is Script=Common, yet
 *  katakana to vim). */
const VIM_CHAR_CLASSES =
  "37e:1,387:1,55a-55f:1,589:1,5be:1,5c0:1,5c3:1,5f3-5f4:1,60c:1,61b:1,61f:1,66a-66d:1,6d4:1,700-70d:1,964-965:1,97" +
  "0:1,df4:1,e4f:1,e5a-e5b:1,f04-f12:1,f3a-f3d:1,f85:1,104a-104f:1,10fb:1,1361-1368:1,166d-166e:1,1680:0,169b-169c:" +
  "1,16eb-16ed:1,1735-1736:1,17d4-17dc:1,1800-180a:1,2000-200b:0,200c-2027:1,2028-2029:0,202a-202e:1,202f:0,2030-20" +
  "3b:1,203c:3,203d-2048:1,2049:3,204a-205e:1,205f:0,2060-206f:1,2070-207f:8304,2080-2094:8320,20a0-2121:1,2122:3,2" +
  "123-2138:1,2139:3,213a-2193:1,2194-2199:3,219a-21a8:1,21a9-21aa:3,21ab-2319:1,231a-231b:3,231c-2327:1,2328:3,232" +
  "9-23ce:1,23cf:3,23d0-23e8:1,23e9-23f3:3,23f4-23f7:1,23f8-23fa:3,23fb-24c1:1,24c2:3,24c3-25a9:1,25aa-25ab:3,25ac-" +
  "25b5:1,25b6:3,25b7-25bf:1,25c0:3,25c1-25fa:1,25fb-25fe:3,25ff:1,2600-2604:3,2605-260d:1,260e:3,260f-2610:1,2611:" +
  "3,2612-2613:1,2614-2615:3,2616-2617:1,2618:3,2619-261c:1,261d:3,261e-261f:1,2620:3,2621:1,2622-2623:3,2624-2625:" +
  "1,2626:3,2627-2629:1,262a:3,262b-262d:1,262e-262f:3,2630-2637:1,2638-263a:3,263b-263f:1,2640:3,2641:1,2642:3,264" +
  "3-2647:1,2648-2653:3,2654-265e:1,265f-2660:3,2661-2662:1,2663:3,2664:1,2665-2666:3,2667:1,2668:3,2669-267a:1,267" +
  "b:3,267c-267d:1,267e-267f:3,2680-2691:1,2692-2697:3,2698:1,2699:3,269a:1,269b-269c:3,269d-269f:1,26a0-26a1:3,26a" +
  "2-26a6:1,26a7:3,26a8-26a9:1,26aa-26ab:3,26ac-26af:1,26b0-26b1:3,26b2-26bc:1,26bd-26be:3,26bf-26c3:1,26c4-26c5:3," +
  "26c6-26c7:1,26c8:3,26c9-26cd:1,26ce-26cf:3,26d0:1,26d1:3,26d2:1,26d3-26d4:3,26d5-26e8:1,26e9-26ea:3,26eb-26ef:1," +
  "26f0-26f5:3,26f6:1,26f7-26fa:3,26fb-26fc:1,26fd:3,26fe-2701:1,2702:3,2703-2704:1,2705:3,2706-2707:1,2708-270d:3," +
  "270e:1,270f:3,2710-2711:1,2712:3,2713:1,2714:3,2715:1,2716:3,2717-271c:1,271d:3,271e-2720:1,2721:3,2722-2727:1,2" +
  "728:3,2729-2732:1,2733-2734:3,2735-2743:1,2744:3,2745-2746:1,2747:3,2748-274b:1,274c:3,274d:1,274e:3,274f-2752:1" +
  ",2753-2755:3,2756:1,2757:3,2758-2762:1,2763-2764:3,2765-2794:1,2795-2797:3,2798-27a0:1,27a1:3,27a2-27af:1,27b0:3" +
  ",27b1-27be:1,27bf:3,27c0-27ff:1,2800-28ff:10240,2900-2933:1,2934-2935:3,2936-2998:1,29d8-29db:1,29fc-29fd:1,2b05" +
  "-2b07:3,2b1b-2b1c:3,2b50:3,2b55:3,2e00-2e7f:1,3000:0,3001-3020:1,3030:3,303d:3,3040-309f:12352,30a0-30ff:12448,3" +
  "297:3,3299:3,3300-9fff:19968,ac00-d7a3:44032,f900-faff:19968,fd3e-fd3f:1,fe30-fe6b:1,ff00-ff0f:1,ff1a-ff20:1,ff3" +
  "b-ff40:1,ff5b-ff65:1,1d000-1d24f:1,1d400-1d7ff:1,1f000-1f003:1,1f004:3,1f005-1f0ce:1,1f0cf:3,1f0d0-1f16f:1,1f170" +
  "-1f171:3,1f172-1f17d:1,1f17e-1f17f:3,1f180-1f18d:1,1f18e:3,1f18f-1f190:1,1f191-1f19a:3,1f19b-1f1e5:1,1f1e6-1f1ff" +
  ":3,1f200:1,1f201-1f202:3,1f203-1f219:1,1f21a:3,1f21b-1f22e:1,1f22f:3,1f230-1f231:1,1f232-1f23a:3,1f23b-1f24f:1,1" +
  "f250-1f251:3,1f252-1f2ff:1,1f300-1f321:3,1f322-1f323:1,1f324-1f393:3,1f394-1f395:1,1f396-1f397:3,1f398:1,1f399-1" +
  "f39b:3,1f39c-1f39d:1,1f39e-1f3f0:3,1f3f1-1f3f2:1,1f3f3-1f3f5:3,1f3f6:1,1f3f7-1f3fa:3,1f3fb-1f3ff:1,1f400-1f4fd:3" +
  ",1f4fe:1,1f4ff-1f53d:3,1f53e-1f548:1,1f549-1f54e:3,1f54f:1,1f550-1f567:3,1f568-1f56e:1,1f56f-1f570:3,1f571-1f572" +
  ":1,1f573-1f57a:3,1f57b-1f586:1,1f587:3,1f588-1f589:1,1f58a-1f58d:3,1f58e-1f58f:1,1f590:3,1f591-1f594:1,1f595-1f5" +
  "96:3,1f597-1f5a3:1,1f5a4-1f5a5:3,1f5a6-1f5a7:1,1f5a8:3,1f5a9-1f5b0:1,1f5b1-1f5b2:3,1f5b3-1f5bb:1,1f5bc:3,1f5bd-1" +
  "f5c1:1,1f5c2-1f5c4:3,1f5c5-1f5d0:1,1f5d1-1f5d3:3,1f5d4-1f5db:1,1f5dc-1f5de:3,1f5df-1f5e0:1,1f5e1:3,1f5e2:1,1f5e3" +
  ":3,1f5e4-1f5e7:1,1f5e8:3,1f5e9-1f5ee:1,1f5ef:3,1f5f0-1f5f2:1,1f5f3:3,1f5f4-1f5f9:1,1f5fa-1f64f:3,1f650-1f67f:1,1" +
  "f680-1f6c5:3,1f6c6-1f6ca:1,1f6cb-1f6d2:3,1f6d3-1f6d4:1,1f6d5-1f6d8:3,1f6d9-1f6db:1,1f6dc-1f6e5:3,1f6e6-1f6e8:1,1" +
  "f6e9:3,1f6ea:1,1f6eb-1f6ec:3,1f6ed-1f6ef:1,1f6f0:3,1f6f1-1f6f2:1,1f6f3-1f6fc:3,1f6fd-1f7df:1,1f7e0-1f7eb:3,1f7ec" +
  "-1f7ef:1,1f7f0:3,1f7f1-1f90b:1,1f90c-1f93a:3,1f93b:1,1f93c-1f945:3,1f946:1,1f947-1f9ff:3,1fa70-1fa7c:3,1fa80-1fa" +
  "8a:3,1fa8e-1fac6:3,1fac8:3,1facd-1fadc:3,1fadf-1faea:3,1faef-1faf8:3,20000-2a6df:19968,2a700-2b81f:19968,2f800-2" +
  "fa1f:19968,";

let vimClassTable: { starts: number[]; ends: number[]; classes: number[] } | null = null;

function vimClassRanges(): { starts: number[]; ends: number[]; classes: number[] } {
  if (vimClassTable !== null) return vimClassTable;
  const table = { starts: [] as number[], ends: [] as number[], classes: [] as number[] };
  for (const entry of VIM_CHAR_CLASSES.split(",")) {
    if (entry === "") continue;
    const [range, cls] = entry.split(":") as [string, string];
    const [from, to] = range.split("-") as [string, string | undefined];
    table.starts.push(parseInt(from, 16));
    table.ends.push(parseInt(to ?? from, 16));
    table.classes.push(Number(cls));
  }
  vimClassTable = table;
  return table;
}

function wordClassOf(code: number): WordClass {
  if (code < 0x100) {
    // vim's default 'iskeyword' (`@,48-57,_,192-255`); every blank, line breaks included, is 0.
    if (/\s/.test(String.fromCharCode(code))) return 0;
    if (/[A-Za-z0-9_\u00b5]/.test(String.fromCharCode(code)) || code >= 0xc0) return 2;
    return 1;
  }
  const { starts, ends, classes } = vimClassRanges();
  let low = 0;
  let high = starts.length - 1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    if (code < starts[mid]!) high = mid - 1;
    else if (code > ends[mid]!) low = mid + 1;
    else return classes[mid]!;
  }
  return 2;
}

/** One step of the word walk: a character -- a whole grapheme cluster (a base with its combining
 *  marks, an emoji ZWJ sequence, a flag), classed by its first code point as vim classes a
 *  character by its base, and landed on at its start -- or a separator between two blocks
 *  (`caret: null`, class 0). */
type WordToken = { cls: WordClass; caret: Caret | null };

type GraphemeSegmenter = { segment(input: string): Iterable<{ index: number }> };

/** `Intl.Segmenter` (WebKit, and Node under jsdom); `null` where it is missing, and then
 *  `fallbackGraphemeStarts` approximates it. Not in this project's `lib` (ES2020), hence the cast. */
const graphemeSegmenter: GraphemeSegmenter | null = (() => {
  const Segmenter = (Intl as unknown as { Segmenter?: new (locale?: string, options?: { granularity: string }) => GraphemeSegmenter }).Segmenter;
  return typeof Segmenter === "function" ? new Segmenter(undefined, { granularity: "grapheme" }) : null;
})();

/** A code point that only ever continues the character before it: a combining mark (variation
 *  selectors included), a zero-width joiner, or an emoji skin-tone modifier. */
const EXTENDER = /^[\p{M}\u200d\p{Emoji_Modifier}]/u;

/** Exported for its own test only: jsdom's Node has `Intl.Segmenter`, so nothing else reaches it there. */
export function fallbackGraphemeStarts(data: string): number[] {
  const starts: number[] = [];
  let at = 0;
  while (at < data.length) {
    starts.push(at);
    const first = data.codePointAt(at)!;
    at += first > 0xffff ? 2 : 1;
    const regional = first >= 0x1f1e6 && first <= 0x1f1ff;
    if (regional && at < data.length) {
      const next = data.codePointAt(at)!;
      if (next >= 0x1f1e6 && next <= 0x1f1ff) at += 2;
    }
    while (at < data.length && EXTENDER.test(data.slice(at, at + 2))) {
      const joiner = data.charCodeAt(at) === 0x200d;
      at += data.codePointAt(at)! > 0xffff ? 2 : 1;
      if (joiner && at < data.length) at += data.codePointAt(at)! > 0xffff ? 2 : 1;
    }
  }
  return starts;
}

const graphemeCache = new WeakMap<Text, { data: string; starts: number[] }>();

/** Where each grapheme cluster of `text` starts, cached per node until its text changes. */
function graphemeStarts(text: Text): number[] {
  const data = text.data;
  const cached = graphemeCache.get(text);
  if (cached !== undefined && cached.data === data) return cached.starts;
  let starts: number[];
  if (graphemeSegmenter !== null) {
    starts = [];
    for (const segment of graphemeSegmenter.segment(data)) starts.push(segment.index);
  } else {
    starts = fallbackGraphemeStarts(data);
  }
  graphemeCache.set(text, { data, starts });
  return starts;
}

const WORD_BLOCK_TAGS = new Set([
  "ADDRESS", "ARTICLE", "ASIDE", "BLOCKQUOTE", "CAPTION", "DD", "DETAILS", "DIV", "DL", "DT", "FIELDSET",
  "FIGCAPTION", "FIGURE", "FOOTER", "FORM", "H1", "H2", "H3", "H4", "H5", "H6", "HEADER", "HR", "LI", "MAIN",
  "NAV", "OL", "P", "PRE", "SECTION", "SUMMARY", "TABLE", "TBODY", "TD", "TFOOT", "TH", "THEAD", "TR", "UL",
]);

/** Elements with no text of their own that still break a line of text where they stand. */
const WORD_BREAK_TAGS = new Set(["BR", "HR", "IMG"]);

/** Whether `el` starts its own block for the word walk: a block tag, or (real layout) any element
 *  whose computed `display` is not inline -- a flex item, say, which a real engine blockifies.
 *  Callers cache the answer per walk, so each element's style is read at most once. */
function isWordBlock(el: Element): boolean {
  if (WORD_BLOCK_TAGS.has(el.tagName)) return true;
  if (typeof getComputedStyle !== "function") return false;
  const display = getComputedStyle(el).display;
  return display !== "" && display !== "contents" && !display.startsWith("inline");
}

function cachedIsWordBlock(el: Element, cache: Map<Element, boolean>): boolean {
  let block = cache.get(el);
  if (block === undefined) {
    block = isWordBlock(el);
    cache.set(el, block);
  }
  return block;
}

/** The nearest block around `node`, up to `root` (itself the outermost block). */
function wordBlockOf(node: Node, root: Node, cache: Map<Element, boolean>): Node {
  for (let el = node.parentElement; el !== null && el !== root; el = el.parentElement) {
    if (cachedIsWordBlock(el, cache)) return el;
  }
  return root;
}

/** `text`'s characters from `offset` on (`forward`, the one containing `offset` first) or wholly
 *  before it (backward), one token each. A cluster that opens the node with a combining mark or a
 *  joiner continues the character the previous node ended on -- highlight.js closes a span between
 *  `e` and its U+0301 -- so it is never a token, or a landing, of its own. */
function* textTokens(text: Text, offset: number, forward: boolean): Generator<WordToken> {
  const data = text.data;
  const starts = graphemeStarts(text);
  // The cluster containing `offset` (the last one starting at or before it).
  let low = 0;
  let high = starts.length - 1;
  let containing = -1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    if (starts[mid]! <= offset) {
      containing = mid;
      low = mid + 1;
    } else high = mid - 1;
  }
  const orphan = (i: number) => i === 0 && EXTENDER.test(data.slice(0, 2));
  const token = (i: number): WordToken => ({ cls: wordClassOf(data.codePointAt(starts[i]!)!), caret: { node: text, offset: starts[i]! } });
  // Whether the containing cluster ends at or before `offset` (`offset` at the node's end).
  const containingEnded = containing >= 0 && (starts[containing + 1] ?? data.length) <= offset;
  if (forward) {
    for (let i = containingEnded ? containing + 1 : Math.max(containing, 0); i < starts.length; i++) if (!orphan(i)) yield token(i);
  } else {
    // Only clusters wholly before `offset`: one `offset` falls inside is the caret's own character.
    for (let i = containingEnded ? containing : containing - 1; i >= 0; i--) if (!orphan(i)) yield token(i);
  }
}

/** `caret` as a position in a text node: itself when it is one, else (an element boundary, which
 *  `Selection.modify` can hand back) the start of the first text node after the boundary, or the end
 *  of the last one in `root` when nothing follows. `null` when `root` holds no text. */
function wordStart(caret: Caret, root: Node): { node: Text; offset: number } | null {
  if (caret.node instanceof Text) return { node: caret.node, offset: Math.min(caret.offset, caret.node.data.length) };
  let after: Node | null = caret.node.childNodes[caret.offset] ?? null;
  if (after === null) {
    let up: Node | null = caret.node;
    while (up !== null && up !== root && up.nextSibling === null) up = up.parentNode;
    after = up === null || up === root ? null : up.nextSibling;
  }
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode: (node: Node) => (isUnrenderedNode(node, root) ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT),
  });
  if (after !== null) {
    if (after instanceof Text) return { node: after, offset: 0 };
    // `after` and everything following it in document order lie past the boundary.
    walker.currentNode = after;
    const next = walker.nextNode();
    if (next !== null) return { node: next as Text, offset: 0 };
  }
  // Nothing follows the boundary: the end of the last text node before it, which is the last one
  // in `root` (the walk restarts from `root`; a failed `nextNode` above left it on `after`).
  walker.currentNode = root;
  let last: Text | null = null;
  for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) last = node as Text;
  return last === null ? null : { node: last, offset: last.data.length };
}

/** The word walk over `root`'s text in document order, forward from `from` (its own character
 *  first) or backward from it (the character before it first). Text nodes are joined across
 *  inline elements (a highlight.js span); a different block, a `<br>`, or `VISUAL_CHROME` between
 *  two text nodes yields one separator token (class 0, like a line break), and chrome's own text is
 *  never a token -- the same chrome `isChromeNode` defines for every other motion. */
function* wordTokens(root: Node, from: { node: Text; offset: number }, forward: boolean): Generator<WordToken> {
  const cache = new Map<Element, boolean>();
  // Whatever is not rendered is rejected whole, its subtree with it (`hidesItsContent`).
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {
    acceptNode: (node: Node) => (hidesItsContent(node) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
  });
  walker.currentNode = from.node;
  let block = wordBlockOf(from.node, root, cache);
  let separated = false;
  yield* textTokens(from.node, from.offset, forward);
  for (let next = forward ? walker.nextNode() : walker.previousNode(); next !== null; next = forward ? walker.nextNode() : walker.previousNode()) {
    if (next instanceof Text) {
      if (next.data.length === 0) continue;
      if (isChromeNode(next, root)) {
        separated = true;
        continue;
      }
      const nextBlock = wordBlockOf(next, root, cache);
      if (separated || nextBlock !== block) yield { cls: 0, caret: null };
      separated = false;
      block = nextBlock;
      yield* textTokens(next, forward ? 0 : next.data.length, forward);
    } else if (
      next instanceof Element &&
      (WORD_BREAK_TAGS.has(next.tagName) || isChromeNode(next, root) || cachedIsWordBlock(next, cache))
    ) {
      // A block passed over between two pieces of text -- an empty one, or a rule -- breaks the line
      // there even with no text of its own; one that holds the next text is a different block anyway.
      separated = true;
    }
  }
}

/** vim's `w` (`fwd_word`): past the rest of the word under the cursor, then past blanks, onto the
 *  next word's first character. With no next word, the last non-blank character it walked over
 *  (vim's `w` from the last word reaches the buffer's end), or `cursor` when there is none. */
function wordForward(cursor: Caret, root: Node): Caret {
  const from = wordStart(cursor, root);
  if (from === null) return cursor;
  const tokens = wordTokens(root, from, true);
  const first = tokens.next();
  if (first.done) return cursor;
  const sclass = first.value.cls;
  let lastNonBlank = sclass === 0 ? null : first.value.caret;
  let t = tokens.next();
  if (sclass !== 0) {
    for (; !t.done && t.value.cls === sclass; t = tokens.next()) lastNonBlank = t.value.caret;
  }
  while (!t.done && t.value.cls === 0) t = tokens.next();
  if (t.done) return lastNonBlank ?? cursor;
  return t.value.caret ?? cursor;
}

/** vim's `e` (`end_word`): one character on; inside the same word, to its last character; else
 *  past blanks to the next word's last character. `cursor` when nothing follows. */
function wordEnd(cursor: Caret, root: Node): Caret {
  const from = wordStart(cursor, root);
  if (from === null) return cursor;
  const tokens = wordTokens(root, from, true);
  const first = tokens.next();
  if (first.done) return cursor;
  const sclass = first.value.cls;
  let t = tokens.next();
  if (!(sclass !== 0 && !t.done && t.value.cls === sclass)) {
    while (!t.done && t.value.cls === 0) t = tokens.next();
  }
  if (t.done) return cursor;
  const cls = t.value.cls;
  let landing = t.value.caret ?? cursor;
  for (t = tokens.next(); !t.done && t.value.cls === cls; t = tokens.next()) landing = t.value.caret ?? landing;
  return landing;
}

/** vim's `b` (`bck_word`): one character back, past blanks, to the first character of the word
 *  there. `cursor` when no word lies before it. */
function wordBackward(cursor: Caret, root: Node): Caret {
  const from = wordStart(cursor, root);
  if (from === null) return cursor;
  const tokens = wordTokens(root, from, false);
  let t = tokens.next();
  while (!t.done && t.value.cls === 0) t = tokens.next();
  if (t.done) return cursor;
  const cls = t.value.cls;
  let landing = t.value.caret ?? cursor;
  for (t = tokens.next(); !t.done && t.value.cls === cls; t = tokens.next()) landing = t.value.caret ?? landing;
  return landing;
}

/** The outermost ancestor of `node` -- the word walk's bound when `runMotion` is called without
 *  `root` (the unit tests' direct calls). */
function topOf(node: Node): Node {
  let top = node;
  while (top.parentNode !== null) top = top.parentNode;
  return top;
}

function readCaret(sel: SelectionLike, fallback: Caret): Caret {
  return sel.focusNode !== null ? { node: sel.focusNode, offset: sel.focusOffset } : fallback;
}

/** The x coordinate of the caret's rectangle, requiring real layout.
 *  Returns null when layout rectangles are unavailable, so `j`/`k` skip goal-column snapping in jsdom.
 *  Real geometry is covered only by the browser harness. */
function caretX(caret: Caret): number | null {
  if (typeof document.createRange !== "function") return null;
  const range = document.createRange();
  range.setStart(caret.node, caret.offset);
  range.collapse(true);
  // jsdom implements `Range` without `getClientRects`, so calling it would throw.
  // Without real layout, goal-column measurement returns null; the browser harness covers geometry.
  if (typeof range.getClientRects !== "function") return null;
  const rect = range.getClientRects()[0];
  return rect === undefined ? null : rect.left;
}

/** After a `j`/`k` step, snap to the character under goal column `x` with `caretRangeFromPoint`.
 *  This is a no-op in jsdom, which lacks that API and passes `caret` through unchanged.
 *
 *  Point hit-testing uses the screen. A line reached outside the list's visible box can
 *  resolve to the composer or other page content. Reveal the landed line with `revealCaret`
 *  before snapping, using the least scroll needed. Accept the snap only inside `root` and
 *  on that line; otherwise keep `modify`'s landing. Without `root`, skip revealing the line
 *  and checking containment. */
function snapToGoalX(caret: Caret, x: number | null, root?: Element): Caret {
  if (x === null || typeof document.caretRangeFromPoint !== "function") return caret;
  if (root !== undefined) {
    if (!root.contains(caret.node)) return caret;
    revealCaret(caret, root);
  }
  const rect = rectOfCaret(caret);
  if (rect === null) return caret;
  const found = document.caretRangeFromPoint(x, (rect.top + rect.bottom) / 2);
  if (found === null) return caret;
  const snapped: Caret = { node: found.startContainer, offset: found.startOffset };
  if (root !== undefined) {
    if (!(snapped.node instanceof Text) || !root.contains(snapped.node)) return caret;
    const snappedRect = rectOfCaret(snapped);
    if (snappedRect === null) return caret;
    const overlap = Math.min(rect.bottom, snappedRect.bottom) - Math.max(rect.top, snappedRect.top);
    if (overlap < Math.min(rect.height, snappedRect.height) / 2) return caret;
  }
  return snapped;
}

/** `caret`'s own rect, or `null` off real layout (jsdom, or a boundary with nothing to measure) --
 *  the same guard `caretX` uses, factored out so `stepByLinePoint` can read a FOUND caret's rect
 *  too, not only the starting one. */
function rectOfCaret(caret: Caret): DOMRect | null {
  if (typeof document.createRange !== "function") return null;
  const range = document.createRange();
  try {
    range.setStart(caret.node, caret.offset);
  } catch {
    return null;
  }
  range.collapse(true);
  if (typeof range.getClientRects !== "function") return null;
  const rect = range.getClientRects()[0] ?? (typeof range.getBoundingClientRect === "function" ? range.getBoundingClientRect() : null);
  if (rect === null || rect === undefined) return null;
  // An all-zero rect is "nothing laid out here" (a detached node, collapsed whitespace), not a real
  // position at the viewport's corner.
  if (rect.width === 0 && rect.height === 0 && rect.top === 0 && rect.left === 0) return null;
  return rect;
}

function isScrollBox(el: Element): { y: boolean; x: boolean } {
  const style = getComputedStyle(el);
  return {
    y: (style.overflowY === "auto" || style.overflowY === "scroll") && el.scrollHeight > el.clientHeight,
    x: (style.overflowX === "auto" || style.overflowX === "scroll") && el.scrollWidth > el.clientWidth,
  };
}

/** keeps `caret` on screen through every scrollable box between it and `list` -- a 260px
 *  `.tool-result-body`, a `.table-scroll` sideways, then the list itself -- each nudged by the least
 *  that shows the caret's own point, never a whole element's `scrollIntoView`. The caret is
 *  re-measured before each box: an inner box's own nudge moves it, and the outer box
 *  deciding from the stale rect scrolled further than it needed to. Returns whether any box moved;
 *  `false` without real layout (jsdom), where it does nothing. `App.tsx`'s `scrollCaretIntoView`
 *  calls this after every region key, and `snapToGoalX`/`stepByLinePoint` before they hit-test the
 *  screen, so the least-scroll rule holds inside a counted motion too. */
export function revealCaret(caret: Caret, list: Element): boolean {
  let el: Element | null = elementOf(caret.node);
  if (el === null || !list.contains(el)) return false;
  let moved = false;
  while (el !== null) {
    const scrolls = isScrollBox(el);
    if (scrolls.x || scrolls.y) {
      const rect = rectOfCaret(caret);
      if (rect === null) return moved;
      const box = el.getBoundingClientRect();
      if (scrolls.y) {
        if (rect.top < box.top) {
          el.scrollTop -= box.top - rect.top;
          moved = true;
        } else if (rect.bottom > box.bottom) {
          el.scrollTop += rect.bottom - box.bottom;
          moved = true;
        }
      }
      if (scrolls.x) {
        if (rect.left < box.left) {
          el.scrollLeft -= box.left - rect.left;
          moved = true;
        } else if (rect.right > box.right) {
          el.scrollLeft += rect.right - box.right;
          moved = true;
        }
      }
    }
    if (el === list) break;
    el = el.parentElement;
  }
  return moved;
}

/** The part of `list` actually on screen, in client coordinates. */
function visibleBand(list: Element): { top: number; bottom: number } {
  const rect = list.getBoundingClientRect();
  const viewport = window.innerHeight || document.documentElement.clientHeight || rect.bottom;
  return { top: Math.max(rect.top, 0), bottom: Math.min(rect.bottom, viewport) };
}

/** Real WebKit line-motion behaviour:
 *  `Selection.modify(..., "line")` does not cross a `<table>` row boundary in WebKitGTK 2.52.6
 *  ("Vjjy over a table copied 'a'" -- the cursor never left row 1), and the same granularity is
 *  what a permission card's own structured body (heading, file line, multi-line diff) tripped on
 *  too. `j`/`k` land by real rendered geometry instead: probe points below (`j`) or above (`k`) the
 *  caret's own rect at the goal column `x`, walking `LINE_STEP_PX` at a time until
 *  `caretRangeFromPoint` resolves to a position genuinely past the STARTING rect's own edge (never
 *  just "a different node" -- two `<td>`s, or a diff's heading and its first line, can share one
 *  visual line), so a row taller than one line of plain text (a wrapped cell, a card control) is
 *  still crossed in a single step rather than landing inside it repeatedly. `null` within
 *  `MAX_LINE_PROBE_PX` means nothing lies further that way -- the same "no movement" the caller
 *  already treats as a boundary, which is what lets `9999j` stop exactly at the transcript's
 *  real last line instead of a granularity failure well short of it. jsdom has
 *  neither `getClientRects` layout nor `caretRangeFromPoint`: this is a no-op there and the
 *  caller falls back to the `Selection.modify` stub the unit tests already exercise. */
const LINE_STEP_PX = 6;
// Keep the probe budget below a typical `.message-list` viewport height to bound how far
// one `j`/`k` looks. `visibleBand` also keeps probes inside the list's visible area, scrolling
// only enough to bring each probe point onto the list.
const MAX_LINE_PROBE_PX = 200;

function stepByLinePoint(cursor: Caret, direction: "forward" | "backward", x: number | null, root?: Element): Caret | null {
  if (x === null || typeof document.caretRangeFromPoint !== "function") return null;
  let startRect = rectOfCaret(cursor);
  if (startRect === null) return null;
  const forward = direction === "forward";
  for (let travelled = 1; travelled <= MAX_LINE_PROBE_PX; travelled += LINE_STEP_PX) {
    let y = forward ? startRect.bottom + travelled : startRect.top - travelled;
    if (root !== undefined) {
      // A probe outside the list's visible band hit-tests other page content, such as the composer.
      // Scroll the list only enough to bring each probe point onto it, one step at a time.
      // If the list cannot scroll further that way, return `null` as at a boundary.
      const band = visibleBand(root);
      const outside = forward ? y - (band.bottom - 1) : band.top - y;
      if (outside > 0) {
        const before = root.scrollTop;
        root.scrollTop = forward ? before + outside : before - outside;
        if (root.scrollTop === before) return null;
        startRect = rectOfCaret(cursor);
        if (startRect === null) return null;
        y = forward ? startRect.bottom + travelled : startRect.top - travelled;
        const after = visibleBand(root);
        if (forward ? y >= after.bottom : y < after.top) return null;
      }
    }
    if (y < 0) break;
    const found = document.caretRangeFromPoint(x, y);
    if (found === null) continue;
    const landed: Caret = { node: found.startContainer, offset: found.startOffset };
    // Only a text position inside the list is a landing: an element container (a `<tr>` between
    // cells, a card's own box) carries a child index, not a character offset.
    if (root !== undefined && (!(landed.node instanceof Text) || !root.contains(landed.node))) continue;
    const landedRect = rectOfCaret(landed);
    if (landedRect === null) continue;
    const pastStart = forward ? landedRect.top >= startRect.bottom - 1 : landedRect.bottom <= startRect.top + 1;
    if (pastStart) return landed;
  }
  return null;
}

/** A wrapped neighbouring cell can make a `<tr>` taller than `MAX_LINE_PROBE_PX`, so the
 *  pixel probe cannot reach the next row even when more rows exist. Step by DOM position
 *  from the caret's enclosing row to its next/previous sibling, keeping the cell index
 *  clamped to the destination row's cell count. This needs no layout and works in jsdom.
 *  Use it only after `Selection.modify` and the pixel probe fail to advance: those preserve
 *  the goal column within a row, while this fallback trades that precision for crossing rows. */
function stepToAdjacentTableRow(cursor: Caret, direction: "forward" | "backward", root?: Element): Caret | null {
  const cell = elementOf(cursor.node)?.closest<HTMLTableCellElement>("td, th") ?? null;
  const row = cell?.closest<HTMLTableRowElement>("tr") ?? null;
  if (cell === null || row === null) return null;
  if (root !== undefined && !root.contains(row)) return null;
  const cellIndex = Array.from(row.cells).indexOf(cell);
  if (cellIndex === -1) return null;
  const sibling = direction === "forward" ? row.nextElementSibling : row.previousElementSibling;
  if (sibling === null || sibling.tagName !== "TR") return null;
  if (root !== undefined && !root.contains(sibling)) return null;
  const siblingCells = (sibling as HTMLTableRowElement).cells;
  if (siblingCells.length === 0) return null;
  const targetCell = siblingCells[Math.min(cellIndex, siblingCells.length - 1)]!;
  return firstSelectableCaret(targetCell);
}

/** The `VisualMotion` values handled by `runMotion`, excluding `gg`/`G`.
 *  `Selection.modify` handles all except `w`/`e`/`b`, which walk DOM text using `wordClassOf`.
 *  `stepOnce` handles `gg`/`G` directly with `firstSelectableCaret`/`lastSelectableCaret`.
 *  Narrowing the type lets its early return enforce exhaustiveness without dead switch cases. */
export type CharMotion = Exclude<VisualMotion, "gg" | "G">;

/** Runs one motion after collapsing to `cursor`; `w`/`e`/`b` walk DOM text using `wordClassOf`.
 *  `stepOnce` handles boundaries and chrome because it has `root`.
 *  Returns the caret read from the live selection and the goal column to carry forward.
 *  Only `j`/`k` set a goal column; all other motions clear it. */
export function runMotion(
  sel: SelectionLike,
  cursor: Caret,
  motion: CharMotion,
  goalX: number | null,
  root?: Element,
): { caret: Caret; goalX: number | null } {
  sel.collapse(cursor.node, cursor.offset);
  switch (motion) {
    case "h":
      sel.modify("move", "backward", "character");
      return { caret: readCaret(sel, cursor), goalX: null };
    case "l":
      sel.modify("move", "forward", "character");
      return { caret: readCaret(sel, cursor), goalX: null };
    case "j":
    case "k": {
      const x = goalX ?? caretX(cursor);
      const forward = motion === "j";
      // Use `Selection.modify(..., "line")` first so counted motion can continue beyond the viewport.
      // If it fails to reach another visual line, such as across table cells or a card's structured
      // body, try `stepByLinePoint`. The goal-column snap and pixel probe hit-test the screen, so
      // `revealCaret`/`visibleBand` reveal each measured position with the least scroll needed.
      sel.modify("move", forward ? "forward" : "backward", "line");
      const byModify = snapToGoalX(readCaret(sel, cursor), x, root);
      const startRect = rectOfCaret(cursor);
      const landedRect = rectOfCaret(byModify);
      const advancedVisually =
        startRect === null || landedRect === null
          ? !caretsEqual(byModify, cursor) // Without real layout in jsdom, trust `modify`'s result.
          : forward
            ? landedRect.top >= startRect.bottom - 1
            : landedRect.bottom <= startRect.top + 1;
      if (advancedVisually) return { caret: byModify, goalX: x };
      const byPoint = stepByLinePoint(cursor, forward ? "forward" : "backward", x, root);
      if (byPoint !== null) return { caret: byPoint, goalX: x };
      // A wrapped neighbouring cell can make a `<tr>` exceed the pixel probe's `MAX_LINE_PROBE_PX`
      // budget. This DOM-position fallback reaches the next/previous row regardless of its height.
      const byRow = stepToAdjacentTableRow(cursor, forward ? "forward" : "backward", root);
      return { caret: byRow ?? byModify, goalX: x };
    }
    case "w":
    case "e":
    case "b": {
      // 2026-10-01: vim's own word rules over the DOM text (`wordForward`/`wordEnd`/
      // `wordBackward`), never `Selection.modify`'s `word` granularity, whose boundaries are not
      // vim's. The live selection is collapsed onto the landing, as `modify` would have left it.
      const scope = root ?? topOf(cursor.node);
      const caret = motion === "w" ? wordForward(cursor, scope) : motion === "e" ? wordEnd(cursor, scope) : wordBackward(cursor, scope);
      sel.collapse(caret.node, caret.offset);
      return { caret, goalX: null };
    }
    case "0":
      sel.modify("move", "backward", "paragraphboundary");
      return { caret: readCaret(sel, cursor), goalX: null };
    case "$":
      sel.modify("move", "forward", "paragraphboundary");
      return { caret: readCaret(sel, cursor), goalX: null };
    default: {
      const _exhaustive: never = motion;
      return _exhaustive;
    }
  }
}

const MAX_CHROME_SKIP = 64;

/** Moves both ends together in CARET; moves only `cursor` in VISUAL/V-LINE, leaving `anchor` fixed.
 *  Every `stepOnce` return uses this helper so a CARET cannot acquire unequal ends and rebuild
 *  as a multi-character selection instead of a one-character block. */
function landBothEnds(model: VisualModel, cursor: Caret, goalX: number | null): VisualModel {
  return model.kind === "caret" ? { ...model, anchor: cursor, cursor, goalX } : { ...model, cursor, goalX };
}

/** Runs one motion step. Leaving `root` undoes the step and returns `moved: false` with the model unchanged.
 *  Landing in chrome retries in the same direction up to `MAX_CHROME_SKIP` times before giving up in place.
 *  `gg`/`G` directly choose the first/last selectable character of `root` (`.message-list` for callers),
 *  bypassing `runMotion` and the chrome-skip loop used for `Selection.modify` boundary crossings. */
export function stepOnce(sel: SelectionLike, model: VisualModel, motion: VisualMotion, root: Element): { model: VisualModel; moved: boolean } {
  if (motion === "gg" || motion === "G") {
    const target = motion === "gg" ? firstSelectableCaret(root) : lastSelectableCaret(root);
    if (target === null || caretsEqual(target, model.cursor)) return { model, moved: false };
    return { model: landBothEnds(model, target, null), moved: true };
  }
  let cursor = model.cursor;
  let goalX = model.goalX;
  for (let attempt = 0; attempt < MAX_CHROME_SKIP; attempt++) {
    let result = runMotion(sel, cursor, motion, goalX, root);
    if (!root.contains(result.caret.node) && (motion === "j" || motion === "k")) {
      // Retry a `j`/`k` step that left `.message-list` with `stepByLinePoint`, anchored at the current
      // caret and kept in the list's visible band. Counted motion can then reach the last line
      // instead of treating an out-of-list landing as the end.
      const byPoint = stepByLinePoint(cursor, motion === "j" ? "forward" : "backward", goalX ?? caretX(cursor), root);
      if (byPoint !== null && root.contains(byPoint.node)) result = { caret: byPoint, goalX: goalX ?? caretX(cursor) };
    }
    if (!root.contains(result.caret.node)) return { model, moved: false };
    if (isChromeNode(result.caret.node, root)) {
      if (caretsEqual(result.caret, cursor)) return { model, moved: false };
      cursor = result.caret;
      goalX = result.goalX;
      continue;
    }
    // A caret with no selectable character at or after it would extend its inclusive block or
    // VISUAL end beyond the list, causing the containment check to refuse `y`/`>`.
    // Clamp it to the last selectable character, as `G` does, including after `$`, `l` or `e`.
    let landed = result.caret;
    if (!hasSelectableTextFrom(landed, root)) {
      const last = lastSelectableCaret(root);
      if (last !== null) landed = last;
    }
    // Leave scrolling to the snap and probe, which reveal measured positions with the least scroll.
    // Centring the caret's parent on every `j`/`k` would jump the view without telling `follow.ts`.
    // `App.tsx` announces whatever the key moved.
    return { model: landBothEnds(model, landed, result.goalX), moved: !caretsEqual(landed, model.cursor) };
  }
  return { model, moved: false };
}

/** Repeats a motion `times`, stopping at the first step that makes no progress.
 *  The caller clamps `times` to `MAX_MOTION_COUNT` through `App.tsx`'s `accumulateMotionCount`,
 *  shared with BROWSE. */
export function repeatMotion(sel: SelectionLike, model: VisualModel, motion: VisualMotion, times: number, root: Element): VisualModel {
  let current = model;
  for (let i = 0; i < times; i++) {
    const { model: next, moved } = stepOnce(sel, current, motion, root);
    current = next;
    if (!moved) break;
  }
  return current;
}

/** `o` swaps which end moves next. It does not call `Selection.modify`;
 *  the next key's `stepOnce` rebuilds from the new `cursor`. */
export function swapEnds(model: VisualModel): VisualModel {
  return { ...model, anchor: model.cursor, cursor: model.anchor, goalX: null };
}

/** The live endpoints set by `rebuildSelection`, used to detect later native selection changes,
 *  such as a mouse drag, before acting on the selection. */
export type BuiltSelection = { anchorNode: Node; anchorOffset: number; focusNode: Node; focusOffset: number };

/** Rebuilds an inclusive live selection from `model` after ordering its ends by document position.
 *  Charwise (`"char"` or `"caret"`) selects from the earlier end through the later end's character
 *  using `extend forward character`. Equal ends therefore highlight one character for CARET.
 *  Linewise (`"line"`) extends from the earlier hard-line start through the later hard-line end
 *  using `paragraphboundary`, also used by `0`/`$`. The native selection is oriented so `extend`
 *  always grows the later end, regardless of the original `anchor`/`cursor` order. */
export function rebuildSelection(sel: SelectionLike, model: VisualModel): BuiltSelection {
  const order = compareCarets(model.anchor, model.cursor);
  const earlier = order <= 0 ? model.anchor : model.cursor;
  const later = order <= 0 ? model.cursor : model.anchor;
  if (model.kind === "line") {
    sel.collapse(earlier.node, earlier.offset);
    sel.modify("move", "backward", "paragraphboundary");
    const lineStart = tableRowEdge(earlier, "start") ?? readCaret(sel, earlier);
    sel.collapse(later.node, later.offset);
    sel.modify("move", "forward", "paragraphboundary");
    const lineEnd = tableRowEdge(later, "end") ?? readCaret(sel, later);
    sel.setBaseAndExtent(lineStart.node, lineStart.offset, lineEnd.node, lineEnd.offset);
  } else {
    sel.setBaseAndExtent(earlier.node, earlier.offset, later.node, later.offset);
    sel.modify("extend", "forward", "character");
  }
  return {
    anchorNode: sel.anchorNode ?? earlier.node,
    anchorOffset: sel.anchorOffset,
    focusNode: sel.focusNode ?? later.node,
    focusOffset: sel.focusOffset,
  };
}

/** V-LINE's hard line inside a table: the ROW, not the cell. `paragraphboundary`
 *  stops at a cell's own edge, so `V j j` over three rows ended inside the third row's first cell
 *  (the real-WebKit W5 copied "a\tb\n1\t2\n3", never the "4" beside the "3" the user saw on that
 *  line). A caret inside a `<td>`/`<th>` widens to its `<tr>`'s first selectable character (`start`)
 *  or just past its last one (`end`); `null` outside a table row, where `paragraphboundary` stands. */
function tableRowEdge(caret: Caret, edge: "start" | "end"): Caret | null {
  const cell = elementOf(caret.node)?.closest("td, th") ?? null;
  const row = cell?.closest("tr") ?? null;
  if (row === null) return null;
  if (edge === "start") return firstSelectableCaret(row);
  const last = lastSelectableCaret(row);
  if (last === null) return null;
  const text = (last.node as Text).data;
  return { node: last.node, offset: text.replace(/\s+$/, "").length };
}

/** Checks that the live selection still matches `rebuildSelection` and both ends remain attached
 *  inside `root`. If a mouse drag or another change replaced it, `y` copies nothing rather than
 *  copying text the user did not see highlighted. */
export function selectionMatchesBuild(sel: SelectionLike, built: BuiltSelection, root: Element): boolean {
  if (sel.anchorNode !== built.anchorNode || sel.anchorOffset !== built.anchorOffset) return false;
  if (sel.focusNode !== built.focusNode || sel.focusOffset !== built.focusOffset) return false;
  if (!built.anchorNode.isConnected || !built.focusNode.isConnected) return false;
  return root.contains(built.anchorNode) && root.contains(built.focusNode);
}

/** Read the highlighted rendered text with `VISUAL_CHROME` hidden by `data-visual-copying`.
 *  Set and remove the attribute synchronously so no frame paints without controls. Mark
 *  rows found through `conversationRows`/`rowOf` and every other direct child of
 *  `.message-list` that the selected range intersects.
 *
 *  The history notice sits above the rows and has a `Copy path` button. A selection extended
 *  there with `k` must hide that button too, without relying on `user-select: none` handling.
 *  Reply HTML stays inside a row and cannot add direct children of the list. */
export function copySelectionText(sel: SelectionLike, root: HTMLElement, model: VisualModel): string {
  const rows = conversationRows(root);
  const anchorRow = rowOf(root, elementOf(model.anchor.node) ?? root);
  const cursorRow = rowOf(root, elementOf(model.cursor.node) ?? root);
  const ai = anchorRow === null ? -1 : rows.indexOf(anchorRow);
  const ci = cursorRow === null ? -1 : rows.indexOf(cursorRow);
  const touched: Element[] = ai === -1 || ci === -1 ? rows : rows.slice(Math.min(ai, ci), Math.max(ai, ci) + 1);
  const list = root.classList.contains("message-list") ? root : root.querySelector(".message-list");
  const range = list === null ? null : selectedRange(sel, model);
  if (list !== null && range !== null) {
    for (const block of Array.from(list.children)) {
      if (!touched.includes(block) && range.intersectsNode(block)) touched.push(block);
    }
  }
  for (const block of touched) block.setAttribute("data-visual-copying", "true");
  const text = sel.toString();
  for (const block of touched) block.removeAttribute("data-visual-copying");
  return text;
}

/** The range `toString()` is about to serialise: the live selection's own when it has one, else
 *  the model's two ends in document order (the same extent, short of V-LINE's own widening to the
 *  ends' hard lines, which never leaves the block either end sits in). */
function selectedRange(sel: SelectionLike, model: VisualModel): Range | null {
  if (sel.rangeCount > 0) return sel.getRangeAt(0);
  if (!model.anchor.node.isConnected || !model.cursor.node.isConnected) return null;
  const order = compareCarets(model.anchor, model.cursor);
  const earlier = order <= 0 ? model.anchor : model.cursor;
  const later = order <= 0 ? model.cursor : model.anchor;
  const range = document.createRange();
  range.setStart(earlier.node, earlier.offset);
  range.setEnd(later.node, later.offset);
  return range;
}

/** Generated table markup contains whitespace-only text nodes between `<tr>`/`<td>` tags.
 *  Checking only `length === 0` would accept them as selectable characters and place V-LINE
 *  above the rendered content. Collapsed whitespace has no `getClientRects()` geometry,
 *  which also breaks the visual-line check in `runMotion`. Reject entirely whitespace nodes. */
function hasSelectableText(node: Node): boolean {
  return (node.textContent ?? "").trim().length > 0;
}

/** The first selectable character inside `container` in document order, excluding chrome.
 *  `container` is the cursor row or a code block reached by HINT through `copyCodeRef`.
 *  The latter starts at the block's first character. `entrySelectableCaret` handles placement
 *  at the list's visible top edge and falls back to this container-first position. */
export function firstSelectableCaret(container: Element): Caret | null {
  const walker = selectableTextWalker(container);
  const first = walker.nextNode();
  return first === null ? null : { node: first, offset: 0 };
}

function selectableTextWalker(container: Element): TreeWalker {
  return document.createTreeWalker(container, NodeFilter.SHOW_TEXT, {
    acceptNode(node: Node) {
      if (!hasSelectableText(node)) return NodeFilter.FILTER_SKIP;
      const el = node.parentElement;
      if (el !== null && el.closest(CHROME_SELECTOR) !== null) return NodeFilter.FILTER_SKIP;
      if (isUnrenderedNode(node, container)) return NodeFilter.FILTER_SKIP;
      return NodeFilter.FILTER_ACCEPT;
    },
  });
}

/** The offset of the last character of `text` that is not whitespace, stepped back over a trailing
 *  surrogate half and combining marks so the caret is never inside one character. `-1` when there
 *  is none. */
function lastCharOffset(text: string): number {
  let offset = text.length - 1;
  while (offset >= 0 && /\s/.test(text[offset]!)) offset--;
  if (offset < 0) return -1;
  // Step back to the start of the last code point, then over any combining marks onto their base.
  for (;;) {
    const code = text.charCodeAt(offset);
    if (code >= 0xdc00 && code <= 0xdfff && offset > 0) {
      const high = text.charCodeAt(offset - 1);
      if (high >= 0xd800 && high <= 0xdbff) offset--;
    }
    const point = String.fromCodePoint(text.codePointAt(offset)!);
    if (offset > 0 && /\p{M}/u.test(point)) {
      offset--;
      continue;
    }
    return offset;
  }
}

/** Find the last selectable character inside `container`, excluding chrome and trailing whitespace.
 *  `TreeWalker` has no reverse entry point that applies `acceptNode`'s chrome filter, so walk
 *  forward like `firstSelectableCaret` and keep the last accepted node.
 *
 *  Land on the character's offset, never one past it. The inclusive caret block and VISUAL
 *  end would otherwise extend beyond the list into the activity line, where the containment
 *  check refuses `y`/`>` and the caret colour does not apply. */
export function lastSelectableCaret(container: Element): Caret | null {
  const walker = selectableTextWalker(container);
  let last: Text | null = null;
  let node: Node | null = walker.nextNode();
  while (node !== null) {
    last = node as Text;
    node = walker.nextNode();
  }
  if (last === null) return null;
  const offset = lastCharOffset(last.data);
  return offset < 0 ? null : { node: last, offset };
}

/** Whether a selectable (non-whitespace, non-chrome) character sits at or after `caret` inside
 *  `root` -- `false` exactly when the caret is past the list's last character.
 *  Cheap in the common case: the caret's own text node answers it unless the caret is at its end,
 *  and then one `nextNode()` does. A caret that is not in a text node is not judged (`true`). */
function hasSelectableTextFrom(caret: Caret, root: Element): boolean {
  if (!(caret.node instanceof Text)) return true;
  const rest = caret.node.data.slice(caret.offset);
  if (/\S/.test(rest) && !isChromeNode(caret.node, root) && !isUnrenderedNode(caret.node, root)) return true;
  const walker = selectableTextWalker(root);
  walker.currentNode = caret.node;
  return walker.nextNode() !== null;
}

/** Enter at the row's first character when its top is visible, otherwise at the first character
 *  under the list's top edge using `caretRangeFromPoint`. `list` is `.message-list`.
 *  Call this for row entry; a HINT landing on a code block uses `firstSelectableCaret` directly.
 *
 *  jsdom has neither layout nor `caretRangeFromPoint`, so it falls back to `firstSelectableCaret`.
 *  Real point placement needs a browser and is exercised by `panel_visual_mode.rs`.
 *  A point outside `container` also falls back to avoid selecting unrelated content. */
export function entrySelectableCaret(container: Element, list: Element | null): Caret | null {
  if (list === null || typeof document.caretRangeFromPoint !== "function") return firstSelectableCaret(container);
  const containerRect = container.getBoundingClientRect();
  const listRect = list.getBoundingClientRect();
  // A row still fully below the list's own top (or a container with no real layout at all, i.e.
  // jsdom's default all-zero rect) needs no point placement -- its own first character is already
  // on screen, or there is nothing to measure against.
  if (containerRect.top >= listRect.top || (containerRect.width === 0 && containerRect.height === 0)) {
    return firstSelectableCaret(container);
  }
  const x = Math.min(Math.max(containerRect.left, listRect.left), listRect.right - 1);
  const y = listRect.top + 1;
  const range = document.caretRangeFromPoint(x, y);
  if (range === null) return firstSelectableCaret(container);
  const node = range.startContainer;
  // At an element edge, subpixel rounding can make `caretRangeFromPoint` return the element
  // itself. Its `startOffset` is then a child index, while this module expects a text offset.
  // Every returned caret must use a `Text` node; anything else falls back as for a missed point.
  if (!(node instanceof Text)) return firstSelectableCaret(container);
  const el = node.parentElement;
  if (el === null || !container.contains(node) || el.closest(CHROME_SELECTOR) !== null) {
    return firstSelectableCaret(container);
  }
  return { node, offset: range.startOffset };
}
