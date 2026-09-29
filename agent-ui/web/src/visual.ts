/**
 * BROWSE's VISUAL mode (spec `docs/superpowers/specs/2026-09-28-browse-visual-mode-design.md`):
 * precise copy over the DOM `Selection.modify` API. Pure DOM logic, no React -- `App.tsx` owns the
 * mode (`PanelMode`'s `"visual"`/`"vline"`), the `VisualModel` this module reads and returns, and
 * every effect (the flash, the clipboard write, ending the mode).
 *
 * jsdom has no `Selection.modify` (D1's own note, spec §3), so every function here that touches a
 * live selection takes a `SelectionLike` rather than the real `Selection` -- a real one satisfies
 * it structurally, and a test fakes it. Everything that only reads plain DOM (`Node`/`Element`/
 * `Range`/`TreeWalker`) works the same in jsdom and a real engine, and is exercised there.
 */
import type { VisualMotion } from "./keymap";
import { conversationRows, rowOf } from "./nav";

/** One boundary point: a DOM position "before" `offset` inside `node`, the same shape
 *  `Selection.collapse`/`setBaseAndExtent` take. Plain data, not a live `Range` -- D3's own rule is
 *  that the DOM selection is rebuilt FROM this on every key, never carried forward as a `Range`. */
export type Caret = { node: Node; offset: number };

/** CARET/VISUAL/V-LINE's whole state (D2-D5, revised for 3a §9): the two ends of the selection --
 *  equal to each other in CARET, where `stepOnce`'s caller moves both -- `kind` (`"caret"` a block
 *  caret, D3; `"char"` VISUAL; `"line"` V-LINE), and the goal column (D5's `j`/`k` table row) -- the
 *  x coordinate of the first vertical move, kept until a horizontal one resets it (`null`). `kind`
 *  replaced a plain `line: boolean` once CARET needed its own third state alongside charwise and
 *  linewise. */
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

/** D6's chrome: a selection may never rest in, or copy from, any of these. Every member also
 *  carries `user-select: none` in `index.css`, and `indexCss.test.ts` holds the two equal so they
 *  cannot drift apart. `.row-sign` and `[aria-hidden="true"]` (the diff gutter, `EditDiff.tsx`)
 *  already carried it before this spec; the rest are new. `.fold-marker` is the run row's own `▸`
 *  (`components/MessageList.tsx`, D6's "fold markers"). */
export const VISUAL_CHROME: string[] = [".row-sign", '[aria-hidden="true"]', "button", "input", "textarea", "[data-nav-action]", ".fold-marker"];

const CHROME_SELECTOR = VISUAL_CHROME.join(", ");

function elementOf(node: Node): Element | null {
  return node instanceof Element ? node : node.parentElement;
}

/** Whether `node` sits inside one of `VISUAL_CHROME`'s elements, bounded by `root` (D6). */
export function isChromeNode(node: Node, root: Element): boolean {
  const el = elementOf(node);
  if (el === null) return false;
  const chrome = el.closest(CHROME_SELECTOR);
  return chrome !== null && root.contains(chrome);
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

/** The character immediately after `caret`, or `null` at a boundary this module does not resolve
 *  further (the end of a text node with no known sibling text, an empty container) -- treated the
 *  same as whitespace by `isWhitespaceAfter` below, since a boundary is as good a word break as any
 *  for `w`'s own purposes (D4). Pure DOM, no `Selection` involved: testable directly in jsdom. */
export function charAfterCaret(caret: Caret): string | null {
  const { node, offset } = caret;
  if (node.nodeType === Node.TEXT_NODE) {
    const text = node.textContent ?? "";
    return offset < text.length ? (text[offset] ?? null) : null;
  }
  const child = node.childNodes[offset];
  return child === undefined ? null : firstCharOf(child);
}

function firstCharOf(node: Node): string | null {
  if (node.nodeType === Node.TEXT_NODE) {
    const text = node.textContent ?? "";
    return text.length > 0 ? (text[0] ?? null) : null;
  }
  for (const child of Array.from(node.childNodes)) {
    const found = firstCharOf(child);
    if (found !== null) return found;
  }
  return null;
}

/** D4's own `w` table row's condition: "on whitespace" means the character right after the caret is
 *  whitespace (or the caret sits at a boundary with no more text to read, treated the same way). */
export function isWhitespaceAfter(caret: Caret): boolean {
  const ch = charAfterCaret(caret);
  return ch === null || /\s/.test(ch);
}

function readCaret(sel: SelectionLike, fallback: Caret): Caret {
  return sel.focusNode !== null ? { node: sel.focusNode, offset: sel.focusOffset } : fallback;
}

/** The x coordinate of `caret`'s own rect -- real layout only; jsdom's `getClientRects()` returns
 *  none, so this reads as `null` there and `j`/`k` simply skip the goal-column snap (R3: unverified
 *  outside the real-WebKit harness, `shell/tests/panel_visual_mode.rs`'s W10/W2 zoom cases). */
function caretX(caret: Caret): number | null {
  if (typeof document.createRange !== "function") return null;
  const range = document.createRange();
  range.setStart(caret.node, caret.offset);
  range.collapse(true);
  // jsdom implements `Range` but not layout: `getClientRects` is simply absent there (it throws
  // "not a function" rather than returning an empty list), so the goal column is a no-op outside a
  // real engine -- R3, verified only in the real-WebKit harness.
  if (typeof range.getClientRects !== "function") return null;
  const rect = range.getClientRects()[0];
  return rect === undefined ? null : rect.left;
}

/** D4's own snap: after a `j`/`k` step, land on the character under the goal column `x`, using
 *  `caretRangeFromPoint` (WebKit; absent in jsdom, where this is a no-op and `caret` passes through
 *  unchanged).
 *
 *  Fix round 2 (item 3a, the review's D8 finding): `caretRangeFromPoint` hit-tests the SCREEN, so a
 *  line `Selection.modify` reached below (or above) the list's visible box -- the next line of a
 *  counted `9999j`, or a plain `j` at the view's bottom edge -- resolved to whatever the page draws
 *  at that point instead: the composer under the list, which is what fix round 1 saw as "`modify`
 *  landing outside the list" and papered over by centring the caret after every step. The landed
 *  line is now revealed first, by the same least nudge `revealCaret` gives D8, and a snap is taken
 *  only when it stays inside `root` and on the landed line; anything else keeps `modify`'s own
 *  landing. `root` absent (the unit tests' direct `runMotion` calls): no reveal, no containment
 *  check, the pre-round-2 behaviour. */
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

/** D8: keeps `caret` on screen through every scrollable box between it and `list` -- a 260px
 *  `.tool-result-body`, a `.table-scroll` sideways, then the list itself -- each nudged by the least
 *  that shows the caret's own point, never a whole element's `scrollIntoView`. The caret is
 *  re-measured before each box (fix round 2): an inner box's own nudge moves it, and the outer box
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

/** Fix round 1 (item 3a, W5/W7's own real-WebKit finding, `/scratch/visual-gui/REPORT.md`):
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
 *  already treats as a boundary (D5/D6), which is what lets `9999j` stop exactly at the transcript's
 *  real last line instead of a granularity failure well short of it (W10's own finding). jsdom has
 *  neither `getClientRects` layout nor `caretRangeFromPoint` (R3): this is a no-op there and the
 *  caller falls back to the `Selection.modify` stub the unit tests already exercise. */
const LINE_STEP_PX = 6;
// Fix round 1 (item 3a, W10's own finding): kept well under a typical `.message-list` viewport
// height rather than a generous, page-scale guess. Since fix round 2 the probe also never leaves the
// list's visible band (`visibleBand`, scrolling the list the least that keeps it there), so this
// only bounds how far one `j`/`k` may look. Still comfortably taller than anything this fallback
// exists for (a wrapped table cell, a permission card's own line).
const MAX_LINE_PROBE_PX = 200;

function stepByLinePoint(cursor: Caret, direction: "forward" | "backward", x: number | null, root?: Element): Caret | null {
  if (x === null || typeof document.caretRangeFromPoint !== "function") return null;
  let startRect = rectOfCaret(cursor);
  if (startRect === null) return null;
  const forward = direction === "forward";
  for (let travelled = 1; travelled <= MAX_LINE_PROBE_PX; travelled += LINE_STEP_PX) {
    let y = forward ? startRect.bottom + travelled : startRect.top - travelled;
    if (root !== undefined) {
      // Fix round 2 (the D8 finding): a probe outside the list's visible band hit-tests whatever
      // the page draws there (the composer, the activity line), so the list itself is scrolled by
      // the least that brings the probe point onto it -- one probe step at a time, so the view
      // moves no further than the line this finds. A list that cannot scroll any further that way
      // has nothing more to find: `null`, the same "no movement" as a real boundary.
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

/** Fix round 3 (item 3a, review finding, minor): a `<tr>` made TALL by a wrapped neighbouring cell
 *  can put the next row's own text further away than `MAX_LINE_PROBE_PX`, so `stepByLinePoint`'s
 *  pixel probe never finds it -- the caret's own cell has nothing further within budget, even though
 *  the table plainly has more rows below. `j`/`k` (and a counted `9999j`) simply stopped inside the
 *  table. This steps by DOM position instead of geometry: from the caret's enclosing `<tr>` to its
 *  next/previous sibling row, landing on the SAME column's cell (clamped to that row's own cell
 *  count) -- so a row's real rendered height can never defeat it. Tried only as a last resort, after
 *  both `Selection.modify` and the pixel probe have already failed to advance: those two are what
 *  preserve `j`/`k`'s own goal column inside a normally-sized row; this one gives that up in exchange
 *  for never getting stuck. Needs no layout at all (no `getClientRects`, no `caretRangeFromPoint`),
 *  so unlike `stepByLinePoint` it runs the same in jsdom as in a real engine. */
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

/** Every `VisualMotion` but `gg`/`G` -- the ones `Selection.modify` actually steps through.
 *  `gg`/`G` are handled entirely in `stepOnce` below, which never calls this for them (D5's own
 *  text: "not `modify("documentboundary")`... none: `firstSelectableCaret` / `lastSelectableCaret`")
 *  -- narrowing the parameter type here, rather than adding two dead cases to the switch below,
 *  is what lets `stepOnce`'s own early return carry the exhaustiveness check instead of duplicating
 *  it. */
export type CharMotion = Exclude<VisualMotion, "gg" | "G">;

/** One motion's own `Selection.modify` steps (D5's table), collapsed to `cursor` first. No boundary
 *  or chrome handling here -- that is `stepOnce`'s job, one level up, since it needs `root` and this
 *  does not. Returns the new caret (read back from the live selection, D5: "read the caret back")
 *  and the goal column to carry forward (only `j`/`k` set one; every other motion clears it, since
 *  the next horizontal move resets `curswant` in vim too). */
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
      // Fix round 1 (item 3a): `Selection.modify(..., "line")` stays the PRIMARY step -- it needs no
      // visible layout, so it is what lets `9999j` keep going past whatever the viewport currently
      // shows all the way to a 300-paragraph transcript's real end. Only when it plainly failed to
      // reach another visual line -- a `<table>` row or a permission card's own structured body,
      // W5/W7's real-WebKit finding -- does `stepByLinePoint`'s real-geometry probe take over. Both
      // the goal-column snap and the probe hit-test the screen, so each reveals what it measures by
      // the least scroll first (fix round 2, `revealCaret`/`visibleBand`), never by centring.
      sel.modify("move", forward ? "forward" : "backward", "line");
      const byModify = snapToGoalX(readCaret(sel, cursor), x, root);
      const startRect = rectOfCaret(cursor);
      const landedRect = rectOfCaret(byModify);
      const advancedVisually =
        startRect === null || landedRect === null
          ? !caretsEqual(byModify, cursor) // no real layout (jsdom, R3): trust `modify`'s own result
          : forward
            ? landedRect.top >= startRect.bottom - 1
            : landedRect.bottom <= startRect.top + 1;
      if (advancedVisually) return { caret: byModify, goalX: x };
      const byPoint = stepByLinePoint(cursor, forward ? "forward" : "backward", x, root);
      if (byPoint !== null) return { caret: byPoint, goalX: x };
      // Fix round 3 (item 3a, review finding, minor): the pixel probe gives up within
      // `MAX_LINE_PROBE_PX`, which a `<tr>` tall enough (a wrapped neighbouring cell) can exceed --
      // this DOM-position fallback finds the next/previous row regardless of how tall it rendered.
      const byRow = stepToAdjacentTableRow(cursor, forward ? "forward" : "backward", root);
      return { caret: byRow ?? byModify, goalX: x };
    }
    case "w":
      // D4: on whitespace, `forward word` then `backward word`; else `forward word` twice then
      // `backward word` -- WebKit's own `word` granularity lands at a word's END moving forward,
      // so this is what turns that into vim's "next word's first character".
      if (isWhitespaceAfter(cursor)) {
        sel.modify("move", "forward", "word");
        sel.modify("move", "backward", "word");
      } else {
        sel.modify("move", "forward", "word");
        sel.modify("move", "forward", "word");
        sel.modify("move", "backward", "word");
      }
      return { caret: readCaret(sel, cursor), goalX: null };
    case "e":
      sel.modify("move", "forward", "character");
      sel.modify("move", "forward", "word");
      sel.modify("move", "backward", "character");
      return { caret: readCaret(sel, cursor), goalX: null };
    case "b":
      sel.modify("move", "backward", "word");
      return { caret: readCaret(sel, cursor), goalX: null };
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

/** D5: both ends move together in CARET (there is only one point); only `cursor` moves in
 *  VISUAL/V-LINE, `anchor` staying put. Shared by every `stepOnce` return path so none of them can
 *  forget it -- a CARET model whose `anchor` drifted from its `cursor` would rebuild as a
 *  multi-character selection instead of D3's one-character block. */
function landBothEnds(model: VisualModel, cursor: Caret, goalX: number | null): VisualModel {
  return model.kind === "caret" ? { ...model, anchor: cursor, cursor, goalX } : { ...model, cursor, goalX };
}

/** One motion step, with D6's boundary rule applied: a step that would leave `root` is undone
 *  (`moved: false`, model unchanged -- vim stops at a buffer's end); a step that lands in chrome
 *  takes another step the same way, up to `MAX_CHROME_SKIP` times, before giving up in place.
 *  `gg`/`G` (D5, added for 3a) are not `Selection.modify` steps at all -- `root` (always
 *  `.message-list`, every caller's own convention) IS the list they place the caret at the first/
 *  last selectable character of, so this returns directly rather than looping through
 *  `runMotion`/chrome-skip, which exist only for `Selection.modify`'s own boundary crossings. */
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
      // Fix round 1 (item 3a, W10's own finding): a `j`/`k` step that left `.message-list` is
      // retried by `stepByLinePoint`'s real-geometry probe, anchored at the CURRENT caret and kept
      // on the list's own visible band (fix round 2), so the list's real last line is where a
      // counted `j` stops, never a step that only looked like the end.
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
    // Fix round 2 (the review's G finding, widened): a caret with no selectable character at or
    // after it in the whole list -- `$` or `l` on the list's last line, `e` into its last word --
    // would draw its block, or extend a VISUAL end, on whatever follows the list, and D9's check
    // then refuses `y`/`>`. It lands on the list's last selectable character instead, as `G` does.
    let landed = result.caret;
    if (!hasSelectableTextFrom(landed, root)) {
      const last = lastSelectableCaret(root);
      if (last !== null) landed = last;
    }
    // Fix round 2 (the review's D8 finding): no scroll here. Fix round 1 centred the caret's parent
    // after every `j`/`k` step, which jumped the view on every key and never told `follow.ts`;
    // the snap and the probe now reveal what they measure by the least scroll themselves, and
    // `App.tsx` announces whatever the key moved.
    return { model: landBothEnds(model, landed, result.goalX), moved: !caretsEqual(landed, model.cursor) };
  }
  return { model, moved: false };
}

/** D5: a count repeats the next motion `times`, stopping at the first step that makes no progress
 *  (`countedStop`'s own rule for rows, applied here to a caret). `times` is already clamped to
 *  `MAX_MOTION_COUNT` by the caller (`App.tsx`'s `accumulateMotionCount`, shared with BROWSE). */
export function repeatMotion(sel: SelectionLike, model: VisualModel, motion: VisualMotion, times: number, root: Element): VisualModel {
  let current = model;
  for (let i = 0; i < times; i++) {
    const { model: next, moved } = stepOnce(sel, current, motion, root);
    current = next;
    if (!moved) break;
  }
  return current;
}

/** `o` (D4, `nvim: visual.txt, v_o`): swap which end moves next. No `Selection.modify` involved --
 *  the next key's `stepOnce` rebuilds from the new `cursor`. */
export function swapEnds(model: VisualModel): VisualModel {
  return { ...model, anchor: model.cursor, cursor: model.anchor, goalX: null };
}

/** What `rebuildSelection` actually set on the live selection, so a later check (D8's "the live
 *  selection still the one VISUAL built") can tell a native selection that changed underneath it
 *  (a mouse drag) apart from one this module itself just built. */
export type BuiltSelection = { anchorNode: Node; anchorOffset: number; focusNode: Node; focusOffset: number };

/** D3/D4: rebuilds the live selection from `model`, vim's inclusive `'selection'`. Charwise
 *  (`"char"` or `"caret"` -- D3's own rule: the caret's block IS "the VISUAL rebuild of D4 with
 *  anchor = cursor"): from the earlier end to the later end plus one character (`extend forward
 *  character`), so both ends' characters are highlighted and a bare caret (anchor === cursor) shows
 *  exactly one. Linewise (`"line"`, V-LINE): from the earlier end's hard-line start to the later
 *  end's hard-line end (`paragraphboundary`, the same unit D5's `0`/`$` use). Always reorders by
 *  document position first -- `anchor`/`cursor` may be in either order, and the native `Selection`
 *  is set up so `extend` always grows the LATER end. */
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

/** V-LINE's hard line inside a table (fix round 2): the ROW, not the cell. `paragraphboundary`
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

/** D8's first check: the live selection is still exactly what `rebuildSelection` last built (a
 *  mouse drag, or anything else, has not replaced it), and both ends are still attached inside
 *  `root`. `y` copies nothing, rather than something the user never actually saw highlighted, when
 *  this is false. */
export function selectionMatchesBuild(sel: SelectionLike, built: BuiltSelection, root: Element): boolean {
  if (sel.anchorNode !== built.anchorNode || sel.anchorOffset !== built.anchorOffset) return false;
  if (sel.focusNode !== built.focusNode || sel.focusOffset !== built.focusOffset) return false;
  if (!built.anchorNode.isConnected || !built.focusNode.isConnected) return false;
  return root.contains(built.anchorNode) && root.contains(built.focusNode);
}

/** D8: the rendered text of what is highlighted, with every `VISUAL_CHROME` member hidden for the
 *  one synchronous read (`data-visual-copying`, set and removed in this same call so no frame ever
 *  paints without chrome). The attribute goes on every block the selection touches -- the rows,
 *  found through `conversationRows`/`rowOf` the same way every other cursor-to-row lookup in this
 *  panel is (`nav.ts`), and every other top-level block of `.message-list` the selected range
 *  crosses.
 *
 *  Fix round 3 (review finding, minor): only rows were marked, but the list holds chrome outside any
 *  row -- the history notice (`HistoryNotice.tsx`, its `Copy path` button) sits above the first row,
 *  and `k` from that row can put an end of the selection in the notice's own text. Its button then
 *  reached `toString()` unshielded, left to WebKit's unverified handling of `user-select: none`
 *  (R2). Every direct child of the list the range intersects is marked now; a reply's own HTML can
 *  never add one (it is inside a row), so this cannot be steered from model output. */
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

/** Item 3a fix round 1 (W5's own real-WebKit finding): a table's generated markup carries
 *  insignificant whitespace-only text nodes between `<tr>`/`<td>` tags (`marked`'s own HTML, not
 *  minified) -- `length === 0` alone let one through as "the first selectable character", landing
 *  V-LINE's own entry a full line ABOVE the table's real content, on a node with no rendered
 *  geometry at all (`getClientRects()` returns none for collapsed whitespace, which is what broke
 *  `runMotion`'s own real-geometry check for `j`/`k`). A node that is entirely whitespace is never a
 *  meaningful landing character, the same rule `isWhitespaceAfter` already applies to `w`'s own
 *  boundary. */
function hasSelectableText(node: Node): boolean {
  return (node.textContent ?? "").trim().length > 0;
}

/** D2's entry caret: the first selectable (non-chrome) character inside `container`, in document
 *  order. `container` is the cursor row, or -- right after a HINT landed on a code block -- that
 *  block itself (`App.tsx`'s own `copyCodeRef`), so entry lands on the block's first character
 *  rather than the row's. Real point-based placement (D2's "the first one under the list's top
 *  edge") is `entrySelectableCaret`, below -- this always picks the container's own first
 *  character, which is what it, and a fully-on-screen row, both fall back to. */
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

/** D5's `gg`/`G` (added for 3a, §9): the LAST selectable (non-chrome) character inside `container`,
 *  in document order -- `TreeWalker` has no "start from the end" entry point that still runs
 *  `acceptNode`'s own chrome filter, so this walks forward the same way `firstSelectableCaret` does
 *  and simply keeps the last accepted node rather than stopping at the first.
 *
 *  Fix round 2 (review finding, important): the caret is ON that character (its offset, never the
 *  text's length). D3's caret is the character after its position; one past the end made CARET's
 *  block, and a VISUAL end's inclusive `+1`, reach past the list into whatever the page draws next
 *  (the activity line), so D9's check refused `y`/`>` and the block was drawn where no caret colour
 *  applies. Trailing whitespace is not a character to land on (a code block's closing newline). */
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
 *  `root` -- `false` exactly when the caret is past the list's last character (fix round 2).
 *  Cheap in the common case: the caret's own text node answers it unless the caret is at its end,
 *  and then one `nextNode()` does. A caret that is not in a text node is not judged (`true`). */
function hasSelectableTextFrom(caret: Caret, root: Element): boolean {
  if (!(caret.node instanceof Text)) return true;
  const rest = caret.node.data.slice(caret.offset);
  if (/\S/.test(rest) && !isChromeNode(caret.node, root)) return true;
  const walker = selectableTextWalker(root);
  walker.currentNode = caret.node;
  return walker.nextNode() !== null;
}

/** D2's on-screen entry rule, fix round 1 (finding 5/6 of both review programs): "the row's first
 *  character when its top is visible, else the first one under the list's top edge
 *  (`caretRangeFromPoint`)". `container` is the same row-or-code-block `firstSelectableCaret` takes;
 *  `list` is `.message-list`, the scroll boundary D6/D7 already use. Only reached for a ROW entry
 *  (`container === row`) -- a HINT-landed code block has no such rule (D2's own text) and always
 *  uses `firstSelectableCaret` directly, so callers that just landed on a code block should call
 *  that, not this.
 *
 *  Real point-based placement is real-browser-only: jsdom has neither layout (`getBoundingClientRect`
 *  reads every rect as all zeros unless a test overrides it, the same convention `scrollCursorRowBox`
 *  documents) nor `document.caretRangeFromPoint` at all, so both are missing there and this falls
 *  back to `firstSelectableCaret` -- exercised by `visual.test.ts`'s own fake, the real behaviour
 *  only by the real-WebKit `panel_visual_mode.rs` (`#[ignore]`d) and a GUI pass. A landing outside
 *  `container` (imprecise geometry at the list's very edge) also falls back, rather than starting
 *  the selection somewhere the caller did not ask for. */
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
  // Item 3a fix round 1 (W7's own real-WebKit finding, at zoom 1.5): `caretRangeFromPoint` at a
  // point that lands right at an element's own edge -- seen at that zoom's own sub-pixel rounding of
  // `containerRect`/`listRect`, which this function's own `>=` check above is not immune to -- can
  // resolve to the ELEMENT itself rather than drilling into a text node, with `startOffset` then a
  // CHILD INDEX, not a character offset (every other function in this module assumes the latter).
  // Every caret this module hands out must be `Text`; anything else falls back the same as a miss.
  if (!(node instanceof Text)) return firstSelectableCaret(container);
  const el = node.parentElement;
  if (el === null || !container.contains(node) || el.closest(CHROME_SELECTOR) !== null) {
    return firstSelectableCaret(container);
  }
  return { node, offset: range.startOffset };
}
