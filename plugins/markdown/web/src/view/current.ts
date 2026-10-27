import { lastTopAtOrBefore, type Box } from "./lineindex";
import type { Preview } from "./preview";
import { yToLine } from "./sync";

/** How long after a click the click's block stays the current one even when the keys arrive later: a click that
 *  gives the preview the keys is seen by the page before or after the host's focus event, and it must win. */
export const CLICK_WINS_MS = 1000;

/* The current block a click (or the keys arriving) picked, while nothing has scrolled since; `line` is nvim's cursor
   line when the block was picked for that cursor: inside the block, or a line next to it that no block holds. */
let clicked: { el: Element; scrollY: number; line: number | null } | null = null;
let clickedAt = Number.NEGATIVE_INFINITY;

/* The document's end sentinel: a tagged element with a line and no end line, last in the index. */
function isEnd(index: Box[], i: number): boolean {
  const el = index[i].el;
  return i === index.length - 1 && el.hasAttribute("data-line") && !el.hasAttribute("data-line-end");
}

/** The block at document position `y`: the index entry whose box holds it, else the first one below it; past the
 *  last block (a short document, read below its end), the last block. */
export function boxAt(index: Box[], y: number): Box | null {
  const i = lastTopAtOrBefore(index, y);
  if (i >= 0 && y < index[i].top + index[i].height && !isEnd(index, i)) return index[i];
  if (i + 1 < index.length && !isEnd(index, i + 1)) return index[i + 1];
  for (let k = Math.min(i, index.length - 1); k >= 0; k--) if (!isEnd(index, k)) return index[k];
  return null;
}

/** The zero-based source line at document position `y`: inside a block the line under `y` (a tall block's lines
 *  share its height), else the first line of the block below. */
export function lineAt(index: Box[], y: number): number | null {
  const box = boxAt(index, y);
  if (!box) return null;
  const inside = y >= box.top && y < box.top + box.height;
  return inside ? Math.min(Math.max(Math.floor(yToLine(index, y)), box.line), box.end - 1) : box.line;
}

/** A clicked block stays current until the next scroll, without scrolling. */
export function setClickedBlock(box: Box | null): void {
  clicked = box ? { el: box.el, scrollY: window.scrollY, line: null } : null;
  clickedAt = box ? performance.now() : Number.NEGATIVE_INFINITY;
}

/** Makes `box`, the block holding nvim's cursor `line` (or the nearest one, for a line no block holds), current as a
 *  click would (until the next scroll), without counting as a click. */
export function pickBlock(box: Box, line: number): void {
  clicked = { el: box.el, scrollY: window.scrollY, line };
}

/** nvim's cursor line when `box` is current because it held that cursor, or was the nearest block to it, as the
 *  keys arrived. */
export function pickedLine(box: Box): number | null {
  return clicked && clicked.el === box.el && clicked.scrollY === window.scrollY ? clicked.line : null;
}

/** Whether a click picked the current block within the last `CLICK_WINS_MS`. */
export function clickedRecently(): boolean {
  return performance.now() - clickedAt < CLICK_WINS_MS;
}

/** The block `yy` and Enter act on: the clicked (or picked) one while nothing has scrolled since, else the block at
 *  the reading position (the viewport's top plus the reading reserve), else the first block below it. */
export function currentBlock(p: Preview): Box | null {
  const index = p.index();
  if (clicked && clicked.scrollY === window.scrollY) {
    const hit = index.find((b) => b.el === clicked!.el);
    if (hit) return hit;
  }
  clicked = null;
  return boxAt(index, p.readingY());
}
