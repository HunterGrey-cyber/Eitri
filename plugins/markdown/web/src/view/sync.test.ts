import { afterEach, describe, expect, it, vi } from "vitest";
import type { Box } from "./lineindex";
import {
  createSettle, createThrottle, HELD_REVEAL_MAX_MS, HELD_REVEAL_QUIET_MS, leader, lineToY, OWN_TOKENS, OwnTokens, placeLine,
  REVEAL_THROTTLE_MS, rowAt, rowShare, scrollTarget, yToLine,
} from "./sync";

const el = document.createElement("div");
const box = (line: number, end: number, top: number, height: number): Box => ({ line, end, top, height, el });
// Lines 0-1 a paragraph, 5-9 a code block, 12 a heading, 20 the end sentinel.
const INDEX = [box(0, 2, 0, 40), box(5, 10, 100, 100), box(12, 13, 260, 20), box(20, 21, 400, 1)];
const FAR = 1e9;

afterEach(() => vi.useRealTimers());

describe("editor to preview", () => {
  it("maps a line inside a block proportionally, and a line in a gap across the gap", () => {
    expect(lineToY(INDEX, 0, FAR)).toBe(0);
    expect(lineToY(INDEX, 1, FAR)).toBe(20);
    expect(lineToY(INDEX, 7, FAR)).toBe(140);
    expect(lineToY(INDEX, 3, FAR)).toBe(60);
    expect(lineToY(INDEX, 11, FAR)).toBe(230);
    expect(lineToY(INDEX, 16.5, FAR)).toBeCloseTo(280 + (120 * 3.5) / 7);
  });
  it("starts at 0 before the first block, ends at the sentinel, and clamps to the scrollable range", () => {
    expect(lineToY(INDEX, -1, FAR)).toBe(0);
    expect(lineToY(INDEX, 25, FAR)).toBe(401);
    expect(lineToY(INDEX, 7, 150)).toBe(140);
    expect(lineToY(INDEX, 11, 150)).toBe(150);
    expect(lineToY([], 5, FAR)).toBe(0);
  });
  it("spreads the lines above the first block over the space above it", () => {
    const index = [box(102, 103, 2400, 1)];
    expect(lineToY(index, 0, FAR)).toBe(0);
    expect(lineToY(index, 51, FAR)).toBe(1200);
    expect(Math.abs(yToLine(index, lineToY(index, 25, FAR)) - 25)).toBeLessThan(1);
  });
  it("puts a line where one block ends and the next begins at the next block's top", () => {
    expect(lineToY([box(0, 3, 0, 60), box(3, 4, 80, 20)], 3, FAR)).toBe(80);
  });
});

describe("one line at the same height on both sides", () => {
  it("puts a line's top the share of nvim's screen its row has down the view", () => {
    // nvim's line 7 starts on row 11 of 41: a quarter down nvim's screen, so a quarter down a 400 px view.
    expect(rowShare(11, 40)).toBe(0.25);
    expect(placeLine(INDEX, 7, rowShare(11, 40), 400, FAR)).toBe(140 - 100);
    // Row 1 is the top edge on both sides: the line's own top, as top-line sync put it.
    expect(placeLine(INDEX, 12, rowShare(1, 40), 400, FAR)).toBe(260);
  });
  it("stops at the document's ends rather than keep the share", () => {
    expect(placeLine(INDEX, 1, 0.5, 400, FAR)).toBe(0);
    expect(placeLine(INDEX, 12, 0, 400, 150)).toBe(150);
  });
  it("finds the row of nvim's screen at a share of the view, the inverse of the share", () => {
    expect(rowAt(100, 400, 40)).toBe(11);
    for (const row of [1, 7, 23, 40]) expect(rowAt(rowShare(row, 40) * 900, 900, 40)).toBe(row);
    expect(rowAt(-30, 400, 40)).toBe(1);
    expect(rowAt(500, 400, 40)).toBe(40);
  });
  it("puts nvim's rows below the part of a cell the editor leaves above its grid, both ways", () => {
    // A 1000 px editor with a 25 px cell and 39 rows leaves 25 px above the grid: a share of 0.025.
    const top = 25 / 1000;
    expect(rowShare(1, 39, top)).toBeCloseTo(0.025, 10);
    expect(rowShare(2, 39, top)).toBeCloseTo(0.05, 10);
    expect(rowShare(39, 39, top)).toBeCloseTo(0.975, 10);
    for (const row of [1, 7, 23, 39]) expect(rowAt(rowShare(row, 39, top) * 1000, 1000, 39, top)).toBe(row);
    // Without it, the share is the rows' alone: the first row a whole remainder high.
    expect(rowShare(1, 39) * 1000).toBe(0);
  });
});

describe("preview to editor", () => {
  it("maps a y inside a block and in a gap to a fractional line", () => {
    expect(yToLine(INDEX, 20)).toBe(1);
    expect(yToLine(INDEX, 140)).toBe(7);
    expect(yToLine(INDEX, 60)).toBe(3);
    expect(yToLine(INDEX, 230)).toBe(11);
    expect(yToLine(INDEX, 1000)).toBe(21);
    expect(yToLine([], 50)).toBe(0);
  });
  it("is the inverse of lineToY within a line", () => {
    for (let L = 0; L <= 20.5; L += 0.25) {
      expect(Math.abs(yToLine(INDEX, lineToY(INDEX, L, FAR)) - L), String(L)).toBeLessThan(1);
    }
    for (let y = 0; y <= 401; y += 7) {
      expect(Math.abs(lineToY(INDEX, yToLine(INDEX, y), FAR) - y), String(y)).toBeLessThan(1);
    }
  });
});

describe("the offset scrolled to", () => {
  it("takes a y a rounding error from a whole pixel for that pixel, and keeps any other fraction", () => {
    expect(scrollTarget(64413.99999999999)).toBe(64414);
    expect(scrollTarget(64414.00000000001)).toBe(64414);
    expect(scrollTarget(64413.557291666664)).toBe(64413.557291666664);
    expect(scrollTarget(0.5)).toBe(0.5);
    expect(scrollTarget(0)).toBe(0);
  });
  it("gives back a whole-pixel offset through yToLine and lineToY over a box at a fraction of a pixel", () => {
    const index = [box(2498, 2499, 64370.671875, 44), box(2500, 2501, 64438.671875, 33.546875)];
    for (let y = 64371; y < 64470; y++) {
      expect(Math.trunc(scrollTarget(lineToY(index, yToLine(index, y), FAR))), String(y)).toBe(y);
    }
  });
});

describe("leadership", () => {
  it("goes to the keys' holder or a pointer scroll, and settings narrow it", () => {
    const cases: [Parameters<typeof leader>[0], string][] = [
      [{ sync: "both", hasKeys: true, pointerScroll: false }, "preview"],
      [{ sync: "both", hasKeys: false, pointerScroll: true }, "preview"],
      [{ sync: "both", hasKeys: false, pointerScroll: false }, "editor"],
      [{ sync: "editor", hasKeys: true, pointerScroll: true }, "editor"],
      [{ sync: "off", hasKeys: true, pointerScroll: false }, "none"],
      [{ sync: "off", hasKeys: false, pointerScroll: false }, "none"],
    ];
    for (const [s, want] of cases) expect(leader(s), JSON.stringify(s)).toBe(want);
  });
});

describe("the loop guards", () => {
  it("know the page's own last eight tokens", () => {
    const own = new OwnTokens();
    const issued = Array.from({ length: OWN_TOKENS + 1 }, () => own.next());
    expect(new Set(issued).size).toBe(OWN_TOKENS + 1);
    expect(own.has(issued[0])).toBe(false);
    for (const t of issued.slice(1)) expect(own.has(t)).toBe(true);
    expect(own.has(999)).toBe(false);
  });
  it("throttle reveals leading and trailing at 40 ms", () => {
    vi.useFakeTimers();
    const fn = vi.fn();
    const t = createThrottle(REVEAL_THROTTLE_MS, fn);
    t.request();
    expect(fn).toHaveBeenCalledTimes(1);
    t.request();
    t.request();
    expect(fn).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(REVEAL_THROTTLE_MS);
    expect(fn).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(REVEAL_THROTTLE_MS * 3);
    expect(fn).toHaveBeenCalledTimes(2);
    t.request();
    expect(fn).toHaveBeenCalledTimes(3);
    t.cancel();
  });
  it("settles a stream of requests: once when it stops, and at most once a max interval while it goes on", () => {
    vi.useFakeTimers();
    const fn = vi.fn();
    const t = createSettle(HELD_REVEAL_QUIET_MS, HELD_REVEAL_MAX_MS, fn);
    // A key repeating 30 times a second for half a second: nothing until it stops.
    for (let i = 0; i < 15; i++) { t.request(); vi.advanceTimersByTime(33); }
    expect(fn).not.toHaveBeenCalled();
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(fn.mock.calls).toEqual([[true]]);
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS * 2);
    expect(fn).toHaveBeenCalledTimes(1);
    // A stream that never pauses is answered once a max interval, then once more when it stops.
    fn.mockClear();
    for (let i = 0; i < 40; i++) { t.request(); vi.advanceTimersByTime(33); }
    expect(fn.mock.calls).toEqual([[false]]);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(fn.mock.calls).toEqual([[false], [true]]);
    // A stream that stops just before its max interval: the max's call, then the stop's.
    fn.mockClear();
    for (let i = 0; i < 31; i++) { t.request(); if (i < 30) vi.advanceTimersByTime(33); }
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS - 990);
    expect(fn.mock.calls).toEqual([[false]]);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(fn.mock.calls).toEqual([[false], [true]]);
    // Cancelled, nothing is left to fire.
    fn.mockClear();
    t.request();
    t.cancel();
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS * 2);
    expect(fn).not.toHaveBeenCalled();
  });
  it("waits longer than the slowest key repeat before it calls a hold over", () => {
    // MAX_REPEAT_MS of the keyboard kit: the slowest repeat interval it believes.
    expect(HELD_REVEAL_QUIET_MS).toBeGreaterThan(100);
  });
});
