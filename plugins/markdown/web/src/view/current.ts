import { lastTopAtOrBefore, type Box } from "./lineindex";
import type { Preview } from "./preview";

let clicked: { el: Element; scrollY: number } | null = null;

/** The block at document position `y`: the index entry whose box holds it, else the first one below it. */
export function boxAt(index: Box[], y: number): Box | null {
  const i = lastTopAtOrBefore(index, y);
  if (i >= 0 && y < index[i].top + index[i].height) return index[i];
  return index[i + 1] ?? null;
}

/** A clicked block stays current until the next scroll, without scrolling. */
export function setClickedBlock(box: Box | null): void {
  clicked = box ? { el: box.el, scrollY: window.scrollY } : null;
}

/** The block `yy` and Enter act on: the clicked one while nothing has scrolled since, else the block at the reading
 *  position (the viewport's top plus one line), else the first block below it. */
export function currentBlock(p: Preview): Box | null {
  const index = p.index();
  if (clicked && clicked.scrollY === window.scrollY) {
    const hit = index.find((b) => b.el === clicked!.el);
    if (hit) return hit;
  }
  clicked = null;
  return boxAt(index, p.readingY());
}
