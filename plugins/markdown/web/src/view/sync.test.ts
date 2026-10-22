import { afterEach, describe, expect, it, vi } from "vitest";
import type { Box } from "./lineindex";
import { createThrottle, leader, lineToY, OWN_TOKENS, OwnTokens, REVEAL_THROTTLE_MS, yToLine } from "./sync";

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
  it("puts a line where one block ends and the next begins at the next block's top", () => {
    expect(lineToY([box(0, 3, 0, 60), box(3, 4, 80, 20)], 3, FAR)).toBe(80);
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
});
