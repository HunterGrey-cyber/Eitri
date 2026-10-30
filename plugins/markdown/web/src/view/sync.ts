/*! Adapted from Visual Studio Code's Markdown preview (extensions/markdown-language-features), Copyright (c) Microsoft Corporation, MIT License. */
import { lastLineAtOrBefore, lastTopAtOrBefore, type Box } from "./lineindex";

/** How often the preview may move nvim while the reader scrolls: often enough to feel attached, rarely enough that
 *  nvim's redraws do not pile up behind a fast scroll. */
export const REVEAL_THROTTLE_MS = 40;
/** How many of the page's own reveal tokens are remembered: nvim answers each reveal with a view carrying its token,
 *  and a fast scroll can have several in flight. */
export const OWN_TOKENS = 8;
/** How long a programmatic scroll's events are taken as the page's own at most, when the position never settles. */
export const GUARD_MS = 150;

/**
 * The document y that shows source line `L` (zero-based, fractional) at the top of the viewport.
 *
 * The anchors are blocks, not the scroll ratio: a line inside a block is placed proportionally inside that block, a
 * line in the gap between two blocks (blank lines, a hidden element) proportionally across the gap, so a tall
 * diagram or a table never drags the lines around it out of place. Lines before the first block share the space
 * above it. A line after the last block maps to its bottom (the end sentinel), and the result is clamped to the
 * range the page can actually scroll to.
 */
export function lineToY(index: Box[], L: number, scrollMax: number): number {
  const clamp = (y: number) => Math.min(Math.max(0, y), Math.max(0, scrollMax));
  const i = lastLineAtOrBefore(index, L);
  if (i < 0) {
    const first = index[0];
    return first && first.line > 0 ? clamp((first.top * Math.max(0, L)) / first.line) : 0;
  }
  const e = index[i];
  if (L < e.end) return clamp(e.top + (e.height * (L - e.line)) / (e.end - e.line));
  const bottom = e.top + e.height;
  const n = index[i + 1];
  if (!n) return clamp(bottom);
  // The next block starts where this one ends: there is no gap to interpolate across.
  if (n.line <= e.end) return clamp(n.top);
  return clamp(bottom + ((n.top - bottom) * (L - e.end)) / (n.line - e.end));
}

/**
 * The fractional source line shown at document y: the inverse of `lineToY` over the same index, so a round trip
 * either way lands within a line of where it started and the two panes never drift apart by repeated syncing. Above
 * the first block the lines before it share the space above it; below the last block the answer is its end line.
 */
export function yToLine(index: Box[], y: number): number {
  if (index.length === 0) return 0;
  const i = lastTopAtOrBefore(index, y);
  if (i < 0) { const first = index[0]; return first.top > 0 ? (first.line * Math.max(0, y)) / first.top : first.line; }
  const e = index[i];
  if (y < e.top + e.height) return e.line + ((e.end - e.line) * (y - e.top)) / e.height;
  const bottom = e.top + e.height;
  const n = index[i + 1];
  if (!n || n.line <= e.end) return e.end;
  return e.end + ((n.line - e.end) * (y - bottom)) / (n.top - bottom);
}

/** How far from a whole pixel an offset may be and still be taken for it: far above floating-point rounding at any
 *  document height, far below anything drawn. */
export const SNAP_PX = 1e-6;

/**
 * The offset to scroll to so that document y `y` is at the top. The browser keeps a scroll offset in whole pixels
 * and drops the fraction, and `lineToY` of the line `yToLine` read at a whole-pixel offset gives that offset back
 * only up to floating-point rounding, as often a hair below it as above: dropped, a hair below loses a pixel. So a y
 * that close to a whole pixel is that pixel, and a line read at an offset scrolls back to the same offset.
 */
export function scrollTarget(y: number): number {
  const whole = Math.round(y);
  return Math.abs(y - whole) < SNAP_PX ? whole : y;
}

export type Leader = "preview" | "editor" | "none";

/** Which side drives the other. Only one side ever does, so the two never chase each other: the side the reader is
 *  using (the keys' holder, or a pointer scroll in the preview), narrowed by the `sync` setting. */
export function leader(s: { sync: "both" | "editor" | "off"; hasKeys: boolean; pointerScroll: boolean }): Leader {
  if (s.sync === "off") return "none";
  if (s.sync === "editor") return "editor";
  return s.hasKeys || s.pointerScroll ? "preview" : "editor";
}

/** The page's recent reveal tokens: a view that echoes one is nvim answering the page, not the reader moving nvim,
 *  and applying it would scroll the preview back to where it was a moment ago. */
export class OwnTokens {
  private recent: number[] = [];
  private last = 0;
  next(): number { const t = ++this.last; this.recent.push(t); if (this.recent.length > OWN_TOKENS) this.recent.shift(); return t; }
  has(token: number): boolean { return this.recent.includes(token); }
}

/** Calls `fn` at once, then at most once per `ms` while requests keep coming, and once more after the last of them,
 *  so the final position always reaches nvim. */
export function createThrottle(ms: number, fn: () => void): { request(): void; cancel(): void } {
  let timer: ReturnType<typeof setTimeout> | undefined; let again = false;
  const tick = () => { if (again) { again = false; fn(); timer = setTimeout(tick, ms); } else timer = undefined; };
  return {
    request() { if (timer === undefined) { fn(); timer = setTimeout(tick, ms); } else again = true; },
    cancel() { if (timer !== undefined) clearTimeout(timer); timer = undefined; again = false; },
  };
}
