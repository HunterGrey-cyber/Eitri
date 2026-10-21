import { jump, maxOf, scrollerOf, scrollToAnimated, type ScrollTargetEl } from "./glide";
import { showMessage } from "./hud";
import type { KitMessage } from "./index";
import { currentSettings, type FindCase } from "./settings";
import { ensureStyle } from "./style";
import { isEscape } from "./events";

/** Elements the engine never draws. */
const UNDRAWN = "script, style, noscript, template, [hidden]";
/** The kit's own elements and text nobody reads as the page's, though the engine may draw them. */
const SKIP = "#eitri-keys-hud, #eitri-keys-sheet, #eitri-keys-find, #eitri-keys-hints, " +
  '[aria-hidden="true"], [aria-hidden=true]';

export const DEFAULT_MESSAGES: Record<KitMessage, string> = {
  "find.notFound": "Pattern not found: {query}",
  "find.none": "no previous search",
  "find.wrapBottom": "search hit BOTTOM, continuing at TOP",
  "find.wrapTop": "search hit TOP, continuing at BOTTOM",
  "find.count": "[{index}/{total}]",
};

/** Whether `query` matches with case: smartcase turns it on as soon as the query holds a capital. */
export function caseSensitive(query: string, mode: FindCase): boolean {
  if (mode === "match") return true;
  if (mode === "ignore") return false;
  return query !== query.toLowerCase();
}

/** Lower case that keeps every UTF-16 unit where it was, so offsets into the folded text are offsets into the page's. */
function fold(text: string): string {
  const lower = text.toLowerCase();
  if (lower.length === text.length) return lower;
  let out = "";
  for (const ch of text) {
    const low = ch.toLowerCase();
    out += low.length === ch.length ? low : ch;
  }
  return out;
}

/** How the search reads an element: `gone` when nothing of it is drawn, `apart` when it is drawn but not searched
 *  (so the text on either side of it does not run together), `block` when its text stands apart from its
 *  neighbours', `inline` when it flows with them. */
type Kind = "gone" | "apart" | "block" | "inline";

function kindOf(el: Element, exclude: string | undefined): Kind {
  if (el.matches(UNDRAWN)) return "gone";
  // A closed details element renders only its summary.
  const parent = el.parentElement;
  if (parent instanceof HTMLDetailsElement && !parent.open && el.tagName !== "SUMMARY") return "gone";
  if (!rendered(el)) return "gone";
  if (el.matches(SKIP)) return "apart";
  if (exclude) {
    try {
      if (el.matches(exclude)) return "apart";
    } catch {
      // A selector the engine cannot parse excludes nothing rather than stopping the search.
    }
  }
  if (el.tagName === "BR") return "apart";
  const display = getComputedStyle(el).display;
  return display === "" || display === "contents" || display.startsWith("inline") || display.startsWith("ruby")
    ? "inline"
    : "block";
}

/** Whether the engine draws `el`. An element with `display: contents` has no box of its own, so `checkVisibility`
 *  says false for it although its children are drawn; its own ancestors decide instead. */
function rendered(el: Element): boolean {
  for (let at: Element | null = el; at; at = at.parentElement) {
    const visible = (at as Element & { checkVisibility?: () => boolean }).checkVisibility?.();
    if (visible !== false) return true;
    if (getComputedStyle(at).display !== "contents") return false;
  }
  return true;
}

/** The rendered text nodes under `root` in document order, with `null` wherever the page shows a break between two
 *  of them: a line break, the edge of a block, or a drawn element the search leaves out. */
function textNodes(root: Element, exclude: string | undefined): (Text | null)[] {
  const out: (Text | null)[] = [];
  const kind = kindOf(root, exclude);
  if (kind === "gone" || kind === "apart") return out;
  // An explicit stack rather than recursion, so a deeply nested page cannot exhaust the call stack.
  const stack: (Node | null)[] = [];
  const push = (parent: Element) => {
    for (let child = parent.lastChild; child; child = child.previousSibling) stack.push(child);
  };
  push(root);
  while (stack.length) {
    const node = stack.pop()!;
    if (node === null) {
      out.push(null);
    } else if (node.nodeType === Node.TEXT_NODE) {
      const parent = node.parentElement;
      if (!(parent instanceof HTMLDetailsElement && !parent.open)) out.push(node as Text);
    } else if (node.nodeType === Node.ELEMENT_NODE) {
      const el = node as Element;
      const how = kindOf(el, exclude);
      if (how === "apart") out.push(null);
      if (how !== "block" && how !== "inline") continue;
      if (how === "block") {
        out.push(null);
        stack.push(null);
      }
      push(el);
    }
  }
  return out;
}

/** Every match of `query` in the rendered text under `root`, as ranges. The text nodes are read as one string, so a
 *  match may cross inline elements, and every run of white space reads as one space, as the page shows it. */
export function matchRanges(root: Element, query: string, exclude: string | undefined, mode: FindCase): Range[] {
  const sensitive = caseSensitive(query, mode);
  const needle = (sensitive ? query : fold(query)).replace(/\s+/g, " ");
  if (!needle) return [];
  const nodes = textNodes(root, exclude);
  // One entry per character of the joined text: the node it came from and where it starts and ends in it. A break
  // between nodes reads as a space that sits at the end of the text before it.
  const parts: string[] = [];
  const nodeOf: Text[] = [];
  const startOf: number[] = [];
  const endOf: number[] = [];
  let lastSpace = false;
  let previous: Text | null = null;
  for (const node of nodes) {
    if (node === null) {
      if (!previous || lastSpace) continue;
      lastSpace = true;
      parts.push(" ");
      nodeOf.push(previous);
      startOf.push(previous.length);
      endOf.push(previous.length);
      continue;
    }
    const data = sensitive ? node.data : fold(node.data);
    for (let i = 0; i < data.length; i++) {
      const space = /\s/.test(data[i]);
      if (space && lastSpace) continue;
      lastSpace = space;
      parts.push(space ? " " : data[i]);
      nodeOf.push(node);
      startOf.push(i);
      endOf.push(i + 1);
    }
    if (data.length) previous = node;
  }
  const hay = parts.join("");
  const ranges: Range[] = [];
  for (let at = hay.indexOf(needle); at >= 0; at = hay.indexOf(needle, at + 1)) {
    const last = at + needle.length - 1;
    const range = root.ownerDocument.createRange();
    range.setStart(nodeOf[at], startOf[at]);
    range.setEnd(nodeOf[last], endOf[last]);
    ranges.push(range);
  }
  return ranges;
}

type HighlightRegistry = { set(name: string, value: unknown): unknown; delete(name: string): unknown };

function highlightApi(): { registry: HighlightRegistry; make: (...ranges: Range[]) => unknown } | null {
  const css = (globalThis as { CSS?: { highlights?: HighlightRegistry } }).CSS;
  const ctor = (globalThis as { Highlight?: new (...ranges: Range[]) => unknown }).Highlight;
  return css?.highlights && typeof ctor === "function"
    ? { registry: css.highlights, make: (...ranges) => new ctor(...ranges) }
    : null;
}

type Options = {
  root?: () => Element | null;
  exclude?: string;
  messages?: Partial<Record<KitMessage, string>>;
  /** Called after the prompt closed, so the HUD shows the mode focus is in now. */
  render?: () => void;
};

let options: Options = {};
let query = "";
let ranges: Range[] = [];
let index = -1;
/** The current match as it was when the kit last moved to it: its range, and its top in document coordinates for
 *  when the range has lost its text. */
let anchor: { range: Range; top: number } | null = null;
let shown = false;
let selected: Range | null = null;
let prompt: HTMLFormElement | null = null;
let restoreFocus: HTMLElement | null = null;

export function configureFind(next: Options): void {
  options = next;
}

function message(name: KitMessage, values: { query?: string; index?: number; total?: number } = {}): string {
  const template = options.messages?.[name] ?? DEFAULT_MESSAGES[name];
  return template.replace(/\{(query|index|total)\}/g, (_, field: "query" | "index" | "total") => String(values[field] ?? ""));
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

function rectOf(range: Range): DOMRect | null {
  const own = typeof range.getBoundingClientRect === "function" ? range.getBoundingClientRect() : null;
  if (own && (own.width || own.height)) return own;
  const box = range.startContainer.parentElement?.getBoundingClientRect();
  return box && (box.width || box.height) ? box : null;
}

/** A match's top in document coordinates; `null` when it has no box (its text is not laid out). */
function topOf(range: Range): number | null {
  const rect = rectOf(range);
  return rect ? rect.top + window.scrollY : null;
}

function sameStart(a: Range, b: Range): boolean {
  return a.startContainer === b.startContainer && a.startOffset === b.startOffset;
}

/** Runs the search again over what the page shows now, and says where the old current match falls among the new
 *  matches: `exact` at an index, or just before index `before` (which may equal the count). */
function rematch(): { exact: number } | { before: number } | null {
  ranges = matchRanges(rootOf(), query, options.exclude, currentSettings().findCase);
  index = -1;
  if (!anchor) return null;
  const old = anchor.range;
  if (old.startContainer.isConnected) {
    const exact = ranges.findIndex((range) => sameStart(range, old));
    if (exact >= 0) return { exact };
    try {
      const after = ranges.findIndex((range) => range.compareBoundaryPoints(Range.START_TO_START, old) > 0);
      return { before: after < 0 ? ranges.length : after };
    } catch {
      // A range from another root cannot be ordered against these; its top still can.
    }
  }
  const tops = ranges.map(topOf);
  const after = tops.findIndex((top) => top !== null && top >= anchor!.top - 1);
  return { before: after < 0 ? ranges.length : after };
}

function setAnchor(): void {
  const range = ranges[index];
  anchor = range ? { range, top: topOf(range) ?? anchor?.top ?? window.scrollY } : null;
}

function paint(): void {
  const api = highlightApi();
  if (api) {
    if (shown && ranges.length) {
      ensureStyle();
      api.registry.set("eitri-find", api.make(...ranges));
      if (ranges[index]) api.registry.set("eitri-find-current", api.make(ranges[index]));
      else api.registry.delete("eitri-find-current");
    } else {
      api.registry.delete("eitri-find");
      api.registry.delete("eitri-find-current");
    }
    return;
  }
  // Without the Custom Highlight API only the current match shows, as the selection: wrapping text in elements
  // would change a DOM the page owns and may rebuild at any moment.
  const selection = document.getSelection();
  if (!selection) return;
  const current = shown ? ranges[index] : undefined;
  if (current) {
    selection.removeAllRanges();
    selection.addRange(current);
    selected = current;
  } else if (selected) {
    if (selection.rangeCount && sameStart(selection.getRangeAt(0), selected)) selection.removeAllRanges();
    selected = null;
  }
}

/** Puts the current match's top a third of the way down its own scroller's view, then moves every scroller around
 *  that one (the window last) whose view the match would still be outside of, the same way, so the match ends up on
 *  screen. Only the innermost scroller glides, since a glide runs on one scroller at a time; the outer ones jump. */
function reveal(): void {
  const range = ranges[index];
  const rect = range && rectOf(range);
  if (!range || !rect) return;
  const chain: ScrollTargetEl[] = [];
  let el = scrollerOf(range.startContainer.parentElement);
  for (;;) {
    chain.push(el);
    if (el === null) break;
    el = scrollerOf(el.parentElement);
  }
  // The match's top in window coordinates once the moves planned so far are made. Moving an outer scroller carries
  // an inner one's view and the match together, so each scroller's move is worked out against its view as it is now.
  let at = rect.top;
  const moves: { el: ScrollTargetEl; top: number }[] = [];
  chain.forEach((scroller, i) => {
    const viewTop = scroller === null ? 0 : scroller.getBoundingClientRect().top;
    const view = scroller === null ? window.innerHeight : scroller.clientHeight;
    if (i > 0 && at >= viewTop && at + Math.min(rect.height, view) <= viewTop + view) return;
    const from = scroller === null ? window.scrollY : scroller.scrollTop;
    const to = Math.round(Math.min(Math.max(0, from + at - viewTop - view / 3), maxOf(scroller)));
    at -= to - from;
    moves.push({ el: scroller, top: to });
  });
  // A jump ends any glide, so the outer jumps come before the inner glide starts.
  for (const move of moves.slice(1)) jump(move.el, move.top);
  scrollToAnimated(moves[0].el, moves[0].top);
}

function showCount(wrap: KitMessage | null): void {
  const count = message("find.count", { query, index: index + 1, total: ranges.length });
  showMessage(wrap ? `${message(wrap, { query })} ${count}` : count);
}

/** The top of the part of the window `node` can show: below every scroller around it that starts lower down. */
function viewTopOf(node: Node): number {
  let top = 0;
  for (let el = scrollerOf(node.parentElement); el; el = scrollerOf(el.parentElement)) {
    top = Math.max(top, el.getBoundingClientRect().top);
  }
  return top;
}

/** Where a match sits against the top of its own scroller's view, in window pixels: negative above it, `null` when
 *  the match has no box. */
function belowView(range: Range): number | null {
  const rect = rectOf(range);
  return rect ? rect.top - viewTopOf(range.startContainer) : null;
}

/** The first match at or below the view's top (`direction` 1), or the last one above it (-1), wrapping. */
function fromView(direction: 1 | -1): KitMessage | null {
  const offsets = ranges.map(belowView);
  if (direction === 1) {
    index = offsets.findIndex((offset) => offset !== null && offset >= -1);
    if (index >= 0) return null;
    index = 0;
    return "find.wrapBottom";
  }
  index = -1;
  offsets.forEach((offset, i) => { if (offset !== null && offset < -1) index = i; });
  if (index >= 0) return null;
  index = ranges.length - 1;
  return "find.wrapTop";
}

function land(wrap: KitMessage | null): void {
  shown = true;
  setAnchor();
  paint();
  reveal();
  showCount(wrap);
}

function notFound(): void {
  paint();
  showMessage(message("find.notFound", { query }));
}

/** `n` (1) or `N` (-1): the next match after the current one, over the text as the page shows it now. */
export function findNext(direction: 1 | -1): void {
  if (!query) {
    showMessage(message("find.none"));
    return;
  }
  const place = rematch();
  const total = ranges.length;
  if (!total) {
    notFound();
    return;
  }
  if (!place) {
    land(fromView(direction));
    return;
  }
  let next: number;
  let wrap: KitMessage | null = null;
  if ("exact" in place) next = place.exact + direction;
  else next = direction === 1 ? place.before : place.before - 1;
  if (next >= total) { next = 0; wrap = "find.wrapBottom"; }
  else if (next < 0) { next = total - 1; wrap = "find.wrapTop"; }
  index = next;
  land(wrap);
}

/** Runs the search again after the page changed its text, keeping the current match the one nearest the old one. */
export function refreshFind(): void {
  if (!query) return;
  const place = rematch();
  if (place && ranges.length) {
    if ("exact" in place) index = place.exact;
    else {
      const tops = ranges.map(topOf);
      let best = -1;
      for (let i = 0; i < tops.length; i++) {
        const top = tops[i];
        if (top === null) continue;
        if (best < 0 || Math.abs(top - anchor!.top) < Math.abs(tops[best]! - anchor!.top)) best = i;
      }
      index = best >= 0 ? best : Math.min(place.before, ranges.length - 1);
    }
    setAnchor();
  }
  paint();
}

/** Sets the search `n` walks, as if the reader had typed it, without moving or highlighting. */
export function seedFind(text: string): void {
  if (typeof text !== "string") return;
  query = text;
  ranges = [];
  index = -1;
  anchor = null;
  shown = false;
  paint();
}

/** Hides the highlights, keeping the search: `n` shows them again. */
export function clearFind(): void {
  shown = false;
  paint();
}

/** Forgets the search and removes the prompt. */
export function resetFind(): void {
  closePrompt(false);
  shown = false;
  paint();
  query = "";
  ranges = [];
  index = -1;
  anchor = null;
  prompt?.remove();
  prompt = null;
  options = {};
}

export function findPromptOpen(): boolean {
  return !!prompt && !prompt.hidden;
}

function run(text: string): void {
  const wanted = text || query;
  if (!wanted) {
    showMessage(message("find.none"));
    return;
  }
  const found = matchRanges(rootOf(), wanted, options.exclude, currentSettings().findCase);
  if (!found.length) {
    // The old search stays, so `n` goes on with it.
    showMessage(message("find.notFound", { query: wanted }));
    return;
  }
  query = wanted;
  ranges = found;
  land(fromView(1));
}

function closePrompt(refocus: boolean): void {
  if (!prompt || prompt.hidden) return;
  prompt.hidden = true;
  const input = prompt.querySelector("input");
  const back = restoreFocus;
  restoreFocus = null;
  if (refocus && back?.isConnected && back !== document.body) back.focus({ preventScroll: true });
  if (document.activeElement === input) input?.blur();
  options.render?.();
}

function makePrompt(): HTMLFormElement {
  const form = document.createElement("form");
  form.id = "eitri-keys-find";
  form.hidden = true;
  const slash = document.createElement("span");
  slash.textContent = "/";
  const input = document.createElement("input");
  input.type = "text";
  input.setAttribute("aria-label", "find");
  input.setAttribute("autocomplete", "off");
  input.spellcheck = false;
  form.append(slash, input);
  form.addEventListener("submit", (ev) => ev.preventDefault());
  input.addEventListener("keydown", (ev) => {
    if (ev.isComposing || ev.keyCode === 229) return;
    if (ev.key === "Enter" && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      const text = input.value;
      closePrompt(true);
      run(text);
    } else if (isEscape(ev)) {
      // Nothing changed while typing, so closing is enough to keep the previous search.
      closePrompt(true);
    } else return;
    ev.preventDefault();
    ev.stopPropagation();
  });
  // Focus leaving the prompt (a click elsewhere) abandons it as Esc does.
  input.addEventListener("blur", () => closePrompt(false));
  return form;
}

/** Opens the `/` prompt; the kit is in insert mode while it has focus. */
export function openFind(): void {
  ensureStyle();
  if (!prompt || !prompt.isConnected) {
    prompt = makePrompt();
    document.body.append(prompt);
  }
  const active = document.activeElement;
  restoreFocus = active instanceof HTMLElement && active !== document.body ? active : null;
  const input = prompt.querySelector("input")!;
  input.value = "";
  prompt.hidden = false;
  input.focus();
}
