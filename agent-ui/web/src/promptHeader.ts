/** The prompt a reader is under, pinned over the top of the conversation while it is scrolled up.
 *
 *  Reading the middle of a long turn -- forty tool calls into it, or half way down a long reply -- the prompt that
 *  started it is pages above, and nothing on screen says what was asked. `MessageList` draws one dim line over the
 *  top of the list naming it: its text cut to one line, then `prompt 4/9`. A click on the line goes to the prompt's
 *  own row.
 *
 *  The header is an overlay, never in the list's flow: the list's height does not change when it appears, so nothing
 *  a reader is looking at moves. It does cover the top of the view, so everything that asks "where does the view
 *  start" asks `effectiveTop` below rather than the list's own top: the cursor's landing and in-row steps
 *  (`./jkScroll`), the visible rows the cursor is re-homed to (`./app/rowScroll`), the HINT and `gf` labels
 *  (`./nav`'s clip boxes), and the reader's anchor across a width change (`./viewAnchor`). A row wholly under the
 *  header counts as off screen.
 *
 *  `headerPrompt` is pure: which prompt, from the timeline and the index of the first row on screen. The DOM half
 *  (where the header is, how much of the view it covers) is at the end. */

import type { TimelineItem } from "./timeline";

/** The prompt the header names: its index in the display timeline, and its place among the prompts that open a
 *  turn (1-based) out of how many the timeline holds. */
export type HeaderPrompt = { index: number; ordinal: number; total: number };

/** A prompt that opens a turn of its own: a top-level `prompt` row the agent did not read inside a turn already
 *  running (a delivered prompt belongs to that turn, and is drawn as part of it). */
function opensTurn(item: TimelineItem): boolean {
  return item.kind === "prompt" && !item.depth && item.delivered !== true;
}

/** Which prompt the header names while the first row on screen is `topIndex`, or `null` for no header:
 *
 *  - while the view follows the newest row (the reader is at the end, where the prompt being answered is recent);
 *  - when the nearest prompt at or above the top row is that row itself (its own row is on screen);
 *  - above the first prompt (the history notice, a session's opening rows);
 *  - when a `wake` row -- a turn the CLI started itself -- lies between that prompt and the top row, the top row
 *    included: the rows under it belong to a turn no prompt row names, and naming the prompt before it would say
 *    the wrong thing.
 *
 *  Delivered prompts are passed over on the way up. `topIndex` outside the timeline (no row on screen) is no header. */
export function headerPrompt(timeline: readonly TimelineItem[], topIndex: number, following: boolean): HeaderPrompt | null {
  if (following || topIndex < 0 || topIndex >= timeline.length) return null;
  let found = -1;
  for (let i = topIndex; i >= 0; i--) {
    const item = timeline[i];
    if (item.depth) continue;
    if (item.kind === "wake") return null;
    if (opensTurn(item)) {
      found = i;
      break;
    }
  }
  if (found === -1 || found === topIndex) return null;
  let ordinal = 0;
  let total = 0;
  for (let i = 0; i < timeline.length; i++) {
    if (!opensTurn(timeline[i])) continue;
    total++;
    if (i <= found) ordinal++;
  }
  return { index: found, ordinal, total };
}

/** The most characters of a prompt the header puts in the page. It shows one line and ends in an ellipsis where the
 *  line ends (`index.css`), so this only keeps a pasted log of a hundred kilobytes out of the DOM. */
export const HEADER_TEXT_MAX = 240;

/** A prompt's text as the header's one line: every run of white space (line breaks included) one space, trimmed,
 *  and no longer than `HEADER_TEXT_MAX` (never splitting a surrogate pair). */
export function headerText(text: string): string {
  const line = text.replace(/\s+/g, " ").trim();
  if (line.length <= HEADER_TEXT_MAX) return line;
  const cut = /[\uD800-\uDBFF]/.test(line[HEADER_TEXT_MAX - 1]) ? HEADER_TEXT_MAX - 1 : HEADER_TEXT_MAX;
  return line.slice(0, cut);
}

/** `prompt 4/9`: the header's count, "prompt" rather than "turn", since a turn the CLI started has no prompt. */
export function headerCount(prompt: HeaderPrompt): string {
  return `prompt ${prompt.ordinal}/${prompt.total}`;
}

// ---------------------------------------------------------------------------------------------
// The DOM half: where the header is, and the top of the view it leaves.
// ---------------------------------------------------------------------------------------------

/** The header's class. `MessageList` draws it as the list's sibling, positioned over the list's top. */
export const PROMPT_HEADER_CLASS = "prompt-header";

type Rect = (el: Element) => { top: number; bottom: number };

const domRect: Rect = (el) => el.getBoundingClientRect();

/** The header drawn over `list`, or `null` while none is. */
export function promptHeaderOver(list: Element): HTMLElement | null {
  const parent = list.parentElement;
  if (parent === null) return null;
  for (const child of Array.from(parent.children)) {
    if (child !== list && child.classList.contains(PROMPT_HEADER_CLASS)) return child as HTMLElement;
  }
  return null;
}

/** How many whole pixels at the top of `list`'s view the header covers: from the list's top to the header's bottom,
 *  never more than the list's own height; `0` with no header. `rect` is the geometry to read, so a test can supply a
 *  layout. */
export function headerInset(list: Element, rect: Rect = domRect): number {
  const header = promptHeaderOver(list);
  if (header === null) return 0;
  const l = rect(list);
  const h = rect(header);
  return Math.round(Math.max(0, Math.min(l.bottom - l.top, h.bottom - l.top)));
}

/** The top of what a reader can see of `list`, in viewport pixels: the list's own top, plus the header's height
 *  while it is shown. The one top every measure of "on screen" in the conversation uses. */
export function effectiveTop(list: Element, rect: Rect = domRect): number {
  return rect(list).top + headerInset(list, rect);
}

/** The header's height before it is measured: the band's editor row, `--nv-editor-row`, 24px until the editor
 *  reports (`index.css`). */
export const HEADER_HEIGHT_PX = 24;

/** How tall the header is, or would be: the shown header's own height, else the `--nv-editor-row` it takes when it
 *  is drawn (the list inherits the property), else `HEADER_HEIGHT_PX`; never more than the list's own height. The
 *  header's decision to show is made from a band of this height whether or not it is shown now, so showing it can
 *  never change which row it is made for. */
export function headerBand(list: Element, rect: Rect = domRect): number {
  const l = rect(list);
  const room = Math.max(0, l.bottom - l.top);
  const shown = headerInset(list, rect);
  if (shown > 0) return shown;
  const declared = parseFloat(getComputedStyle(list).getPropertyValue("--nv-editor-row"));
  return Math.round(Math.min(room, declared > 0 ? declared : HEADER_HEIGHT_PX));
}

/** The index in `rows` of the first row on screen: the first whose bottom is below `top` and whose top is above
 *  `bottom`, or `-1` with none. Rows are stacked in order, so a binary search finds it. `shift` moves every row
 *  that many pixels first (a view about to be scrolled by `-shift`). */
export function firstRowBelow(rows: readonly Element[], top: number, bottom: number, rect: Rect = domRect, shift = 0): number {
  let lo = 0;
  let hi = rows.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (rect(rows[mid]).bottom + shift <= top) lo = mid + 1;
    else hi = mid;
  }
  return lo < rows.length && rect(rows[lo]).top + shift < bottom ? lo : -1;
}

/** How much of the top of `list` the header will cover once a key has placed a row, so the key can land the row
 *  below it: `0` where no header will show, else `headerBand`. `rows` are the list's conversation rows (index `i`
 *  is timeline index `i`) and `scrollFor(inset)` is how far the list is scrolled to put the row where the key
 *  wants it when `inset` pixels at the top are covered. The header is decided as the list's scroll handler will
 *  decide it after the move -- from the first row below the band, with the view no longer following -- for a
 *  landing that leaves the top free and for one that leaves the band free; either one showing it, the row lands
 *  below the band, which is right whether or not the header then shows. */
export function landingInset(
  list: Element,
  rows: readonly Element[],
  timeline: readonly TimelineItem[],
  scrollFor: (inset: number) => number,
  rect: Rect = domRect,
): number {
  const band = headerBand(list, rect);
  if (band <= 0) return 0;
  const l = rect(list);
  for (const inset of [0, band]) {
    const first = firstRowBelow(rows, l.top + band, l.bottom, rect, -scrollFor(inset));
    if (first !== -1 && headerPrompt(timeline, first, false) !== null) return band;
  }
  return 0;
}
