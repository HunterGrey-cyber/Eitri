// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cancelListScroll,
  decideLanding,
  isListScrollAnimating,
  readBoxState,
  readRowGeometry,
  scrollListTo,
  SCROLL_ANIMATION_MS,
  settleListScroll,
} from "./jkScroll";

/* The DOM half of `jkScroll.ts`: reading a list's geometry as whole pixels, and writing `scrollTop`
   with the ease-out. jsdom has no layout, so every number here is planted by the test; what these pin
   is the arithmetic around the engine's own numbers (the 900.391px rect against a 900px
   `clientHeight`), not what a real WebKit draws -- `shell/tests/panel_jk_scroll.rs` does that. */

function fakeList(opts: { scrollTop?: number; clientHeight?: number; rectHeight?: number; scrollHeight?: number }) {
  const list = document.createElement("div");
  let scrollTop = opts.scrollTop ?? 0;
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = v;
    },
  });
  Object.defineProperty(list, "clientHeight", { configurable: true, value: opts.clientHeight ?? 0 });
  Object.defineProperty(list, "scrollHeight", { configurable: true, value: opts.scrollHeight ?? 0 });
  list.getBoundingClientRect = () => ({ top: 10, bottom: 10 + (opts.rectHeight ?? 0) }) as DOMRect;
  return list;
}

function rowAt(top: number, bottom: number, lineHeight = "24.75px") {
  const row = document.createElement("div");
  const body = document.createElement("div");
  body.className = "row-body";
  body.style.lineHeight = lineHeight;
  row.appendChild(body);
  row.getBoundingClientRect = () => ({ top, bottom }) as DOMRect;
  return row;
}

describe("readRowGeometry", () => {
  it("measures the view as clientHeight, not the fractional rect: 900 for a 900.391px list", () => {
    const list = fakeList({ clientHeight: 900, rectHeight: 900.391, scrollHeight: 6570, scrollTop: 100 });
    const geometry = readRowGeometry(list, rowAt(10 + 900.391 - 100, 10 + 900.391 - 100 + 1475))!;
    expect(geometry.view).toEqual({ scrollTop: 100, height: 900, maxScrollTop: 5670 });
    expect(geometry.row).toEqual({ top: 900, bottom: 2375 }); // content space: rect - list top + scrollTop, rounded
    expect(geometry.line).toBeCloseTo(24.75, 5);
  });

  it("falls back to the rounded rect where an environment reports no clientHeight", () => {
    const list = fakeList({ clientHeight: 0, rectHeight: 799.6 });
    const geometry = readRowGeometry(list, rowAt(10, 110))!;
    expect(geometry.view.height).toBe(800);
    expect(geometry.view.maxScrollTop).toBe(Number.POSITIVE_INFINITY);
  });

  it("has no geometry when the list has no height (no layout to judge)", () => {
    expect(readRowGeometry(fakeList({ rectHeight: 0 }), rowAt(0, 100))).toBeNull();
  });

  it("the old dead press: a tall row whose top is a fraction below the view's bottom is now entered, at zoom 1.0", () => {
    // Content space: the previous short row ends at 900.39 in the list's own rect (clientHeight 900).
    const list = fakeList({ clientHeight: 900, rectHeight: 900.391, scrollHeight: 6570, scrollTop: 0 });
    const row = rowAt(10 + 900.39, 10 + 900.39 + 1475);
    const geometry = readRowGeometry(list, row)!;
    const landing = decideLanding({ direction: 1, view: geometry.view, row: geometry.row, line: geometry.line });
    expect(landing.how).toBe("head");
    expect(landing.scrollTop).toBe(600);
  });

  it("and at zoom 1.5 (903.74px rect, clientHeight 904) it takes the same branch rather than a whole view", () => {
    const list = fakeList({ clientHeight: 904, rectHeight: 903.74, scrollHeight: 6660, scrollTop: 0 });
    const row = rowAt(10 + 903.74, 10 + 903.74 + 1451);
    const geometry = readRowGeometry(list, row)!;
    const landing = decideLanding({ direction: 1, view: geometry.view, row: geometry.row, line: geometry.line });
    expect(landing.how).toBe("head");
    expect(landing.scrollTop).toBe(603);
  });
});

describe("readBoxState", () => {
  it("puts the box in the list's content space, with its own scroll state", () => {
    const list = fakeList({ clientHeight: 900, rectHeight: 900, scrollTop: 50 });
    const box = document.createElement("pre");
    box.getBoundingClientRect = () => ({ top: 210, bottom: 470 }) as DOMRect;
    Object.defineProperty(box, "scrollTop", { configurable: true, value: 36 });
    Object.defineProperty(box, "clientHeight", { configurable: true, value: 260 });
    Object.defineProperty(box, "scrollHeight", { configurable: true, value: 2200 });
    expect(readBoxState(list, box)).toEqual({ top: 250, bottom: 510, scrollTop: 36, clientHeight: 260, scrollHeight: 2200 });
  });
});

describe("scrollListTo -- the ease-out", () => {
  let now = 0;
  let nextId = 1;
  let queue = new Map<number, FrameRequestCallback>();
  let reduced = false;

  /** Runs the animation frames due within `ms` of fake time, one 16ms frame at a time. */
  function advance(ms: number) {
    const end = now + ms;
    while (now < end) {
      now = Math.min(end, now + 16);
      const due = [...queue.entries()];
      queue = new Map();
      for (const [, callback] of due) callback(now);
    }
  }

  beforeEach(() => {
    now = 1000;
    nextId = 1;
    queue = new Map();
    reduced = false;
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      const id = nextId++;
      queue.set(id, cb);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) => {
      queue.delete(id);
    });
    vi.spyOn(performance, "now").mockImplementation(() => now);
    vi.stubGlobal("matchMedia", (query: string) => ({ matches: reduced && query.includes("reduced-motion") }));
    (window as unknown as { matchMedia: unknown }).matchMedia = (query: string) => ({
      matches: reduced && query.includes("reduced-motion"),
    });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("moves in frames and reaches the target after the animation's length, and only then", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 700, true);
    // The first frame's worth of the move is written at once (see the next test): the list is on its way,
    // not yet at its target.
    expect(list.scrollTop).toBeGreaterThan(100);
    expect(list.scrollTop).toBeLessThan(700);
    expect(isListScrollAnimating(list)).toBe(true);
    // the first frame's share was written at the call, so the ease ends one frame (16ms) before 150ms
    advance(SCROLL_ANIMATION_MS - 48);
    expect(list.scrollTop).toBeGreaterThan(100);
    expect(list.scrollTop).toBeLessThan(700);
    expect(isListScrollAnimating(list)).toBe(true);
    advance(64);
    expect(list.scrollTop).toBe(700);
    expect(isListScrollAnimating(list)).toBe(false);
  });

  it("starts moving in the same task as the call, so a scroll event already queued sees the view gone from where it was", () => {
    // A queued scroll event from the follow-the-newest snap is delivered before the first animation frame:
    // found at the bottom it re-arms following, undoing the `k` that began this move. The first frame's
    // share of the move is therefore written now, for a move of any size worth easing.
    for (const [from, to] of [
      [1000, 700],
      [1000, 1003],
      [0, 2],
      [500, 0],
    ]) {
      const list = fakeList({ scrollTop: from });
      scrollListTo(list, to, true);
      expect(list.scrollTop).not.toBe(from);
      expect(Math.sign(list.scrollTop - from)).toBe(Math.sign(to - from));
      cancelListScroll(list);
    }
  });

  it("eases out: more than half the way in the first half of the time, never backwards", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    const seen: number[] = [];
    for (let i = 0; i < 12; i++) {
      advance(16);
      seen.push(list.scrollTop);
    }
    for (let i = 1; i < seen.length; i++) expect(seen[i]).toBeGreaterThanOrEqual(seen[i - 1]);
    const half = seen[Math.floor(SCROLL_ANIMATION_MS / 2 / 16) - 1];
    expect(half).toBeGreaterThan(300);
    expect(list.scrollTop).toBe(600);
  });

  it("is immediate for a key repeat, a count, G/gg: anything that asks for no animation", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 700, false);
    expect(list.scrollTop).toBe(700);
    expect(isListScrollAnimating(list)).toBe(false);
    expect(queue.size).toBe(0);
  });

  it("is immediate when the system asks for reduced motion", () => {
    reduced = true;
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 700, true);
    expect(list.scrollTop).toBe(700);
    expect(queue.size).toBe(0);
  });

  it("is immediate for a move too small to see", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 101, true);
    expect(list.scrollTop).toBe(101);
    expect(queue.size).toBe(0);
    scrollListTo(list, 107, true);
    expect(list.scrollTop).toBe(107);
    expect(queue.size).toBe(0);
  });

  it("an eased move's first write is at least two pixels, past the tolerance a queued scroll event reads the bottom by", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 108, true);
    expect(Math.abs(list.scrollTop - 100)).toBeGreaterThanOrEqual(2);
  });

  it("writes nothing for a target the list is already at", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 100, true);
    expect(queue.size).toBe(0);
    expect(isListScrollAnimating(list)).toBe(false);
  });

  it("settleListScroll puts the list at the running animation's target now, and stops it", () => {
    const list = fakeList({ scrollTop: 100 });
    scrollListTo(list, 700, true);
    advance(48);
    expect(list.scrollTop).toBeLessThan(700);
    settleListScroll(list);
    expect(list.scrollTop).toBe(700);
    expect(isListScrollAnimating(list)).toBe(false);
    advance(200);
    expect(list.scrollTop).toBe(700); // the cancelled frame never runs
  });

  it("settleListScroll with nothing running changes nothing", () => {
    const list = fakeList({ scrollTop: 321 });
    settleListScroll(list);
    expect(list.scrollTop).toBe(321);
  });

  it("a press during an animation starts from that animation's target: settle, then the next scroll", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 300, true);
    advance(48);
    settleListScroll(list); // what the key handler does first
    expect(list.scrollTop).toBe(300);
    scrollListTo(list, 600, true);
    advance(SCROLL_ANIMATION_MS + 32);
    expect(list.scrollTop).toBe(600);
  });

  it("yields to anyone else writing the list: the follow-the-newest snap, a wheel, a restored view", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    advance(48);
    list.scrollTop = 5000; // something else moved the list
    advance(200);
    expect(list.scrollTop).toBe(5000); // never pulled back toward the animation's own line
    expect(isListScrollAnimating(list)).toBe(false);
  });

  it("is not 'easing' once somebody else has moved the list, even before the next frame: the scroll event it causes is theirs", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    advance(48);
    expect(isListScrollAnimating(list)).toBe(true);
    list.scrollTop = 5000; // a wheel
    expect(isListScrollAnimating(list)).toBe(false);
  });

  it("cancelListScroll drops an animation without moving the list, and its frames never run", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    advance(32);
    const at = list.scrollTop;
    cancelListScroll(list);
    expect(isListScrollAnimating(list)).toBe(false);
    advance(300);
    expect(list.scrollTop).toBe(at);
  });

  it("cancelListScroll stops an animation that began at the position a restored view is about to write again", () => {
    // A tab's saved view that happens to equal where the old tab's animation started looks, to the "did
    // somebody else write?" check, like nobody wrote at all: only an explicit cancel ends it.
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 227, true);
    list.scrollTop = 0; // the restore
    cancelListScroll(list);
    advance(300);
    expect(list.scrollTop).toBe(0);
  });

  it("settleListScroll leaves a list somebody else has moved where it is", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    advance(48);
    list.scrollTop = 5000;
    settleListScroll(list);
    expect(list.scrollTop).toBe(5000);
  });

  it("a second scroll replaces the first, from wherever the list is", () => {
    const list = fakeList({ scrollTop: 0 });
    scrollListTo(list, 600, true);
    advance(48);
    const mid = list.scrollTop;
    scrollListTo(list, 100, true);
    advance(SCROLL_ANIMATION_MS + 32);
    expect(mid).toBeGreaterThan(100);
    expect(list.scrollTop).toBe(100);
  });
});
