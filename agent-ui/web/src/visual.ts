/**
 * BROWSE's VISUAL mode copies text using the DOM `Selection.modify` API.
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

/** The character after `caret`, or null at an unresolved boundary such as an exhausted text node
 *  or empty container. `isWhitespaceAfter` treats null as whitespace so `w` can use it as a word break.
 *  Uses only the DOM, with no live `Selection`, so it can be tested directly in jsdom. */
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

/** For `w`, whitespace means the character after the caret is whitespace or there is no more text
 *  at that boundary. Both cases are treated alike. */
export function isWhitespaceAfter(caret: Caret): boolean {
  const ch = charAfterCaret(caret);
  return ch === null || /\s/.test(ch);
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

/** The `VisualMotion` values handled by `Selection.modify`, excluding `gg`/`G`.
 *  `stepOnce` handles those directly with `firstSelectableCaret`/`lastSelectableCaret`.
 *  Narrowing the type lets its early return enforce exhaustiveness without dead switch cases. */
export type CharMotion = Exclude<VisualMotion, "gg" | "G">;

/** Runs one motion's `Selection.modify` steps after collapsing to `cursor`.
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
      // On whitespace, use `forward word` then `backward word`; otherwise use two forward word steps
      // then one backward step. Forward word movement lands at a word's end, so this composition
      // aims for the next word's first character.
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
  if (/\S/.test(rest) && !isChromeNode(caret.node, root)) return true;
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
