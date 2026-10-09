/** The held `j`/`k` glide as numbers (`HoldMotion`): a steady speed whatever the rows, the cursor on the
 *  row under its line, the stop on release, the ends. The DOM half is driven through `App` in
 *  `App.jkScroll.test.tsx`. */
import { describe, expect, it } from "vitest";
import {
  DEFAULT_REPEAT_MS,
  HOLD_LINES_PER_REPEAT,
  HOLD_RAMP_MS,
  HoldMotion,
  holdSpeed,
  MAX_STALL_MS,
  MIN_STALL_MS,
  rowAtLine,
  stallAfter,
} from "./jkHold";
import { decideLanding, READING_LINE } from "./jkScroll";
import type { Direction, Span } from "./jkScroll";

const LINE = 20;
const HEIGHT = 600;

/** Rows stacked at `heights` from 0, with no gaps. */
function stack(heights: number[]): Span[] {
  let y = 0;
  return heights.map((h) => {
    const span = { top: y, bottom: y + h };
    y += h;
    return span;
  });
}

const total = (rows: Span[]) => rows[rows.length - 1].bottom;

/** Rows of very different heights: one-liners, a 2,000px essay, a 900px tool result, a 30px prompt. */
const MIXED = [40, 60, 2000, 30, 900, 45, 30, 1200, 80, 30, 700, 50, 40, 60, 2000, 30];
/** The same, three times over: long enough to glide for seconds without reaching the end. */
const LONG = [...MIXED, ...MIXED, ...MIXED];

function motion(input: { rows: Span[]; cursor: number; scrollTop: number; direction: Direction; speed?: number }) {
  return new HoldMotion({
    direction: input.direction,
    rows: input.rows,
    cursor: input.cursor,
    view: { scrollTop: input.scrollTop, height: HEIGHT, maxScrollTop: total(input.rows) - HEIGHT },
    line: LINE,
    speed: input.speed ?? holdSpeed(LINE, DEFAULT_REPEAT_MS),
    now: 0,
  });
}

describe("holdSpeed and stallAfter", () => {
  it("is three lines per repeat interval, never a row's height", () => {
    expect(holdSpeed(LINE, 40)).toBeCloseTo((HOLD_LINES_PER_REPEAT * LINE) / 40, 10);
    expect(holdSpeed(LINE, DEFAULT_REPEAT_MS) * DEFAULT_REPEAT_MS).toBeCloseTo(3 * LINE, 10);
  });

  it("holds an absurd measured interval to its bounds", () => {
    expect(holdSpeed(LINE, 1)).toBe(holdSpeed(LINE, 20));
    expect(holdSpeed(LINE, 5000)).toBe(holdSpeed(LINE, 100));
  });

  it("waits about two repeat intervals for the next repeat, within bounds", () => {
    expect(stallAfter(50)).toBe(100);
    expect(stallAfter(10)).toBe(MIN_STALL_MS);
    expect(stallAfter(500)).toBe(MAX_STALL_MS);
  });
});

describe("rowAtLine", () => {
  const rows = [
    { top: 0, bottom: 100 },
    { top: 110, bottom: 200 },
    { top: 200, bottom: 400 },
  ];

  it("is the row the line is in", () => {
    expect(rowAtLine(rows, 0, 1)).toBe(0);
    expect(rowAtLine(rows, 150, 1)).toBe(1);
    expect(rowAtLine(rows, 399, -1)).toBe(2);
  });

  it("in a gap, is the row just left: above going down, below going up", () => {
    expect(rowAtLine(rows, 105, 1)).toBe(0);
    expect(rowAtLine(rows, 105, -1)).toBe(1);
  });

  it("clamps above the first row and below the last", () => {
    expect(rowAtLine(rows, -50, 1)).toBe(0);
    expect(rowAtLine(rows, 900, 1)).toBe(2);
  });
});

describe("a held j: the speed", () => {
  it("is constant across rows of very different heights once it has gathered", () => {
    const rows = stack(LONG);
    const speed = holdSpeed(LINE, DEFAULT_REPEAT_MS);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    let t = 0;
    // Gather speed and let the line reach its reading line.
    while (t < 2000) m.advance((t += 16));
    const lineSteps: number[] = [];
    const viewSteps: number[] = [];
    const passed = new Set<number>();
    let before = m.state;
    for (let i = 0; i < 120; i++) {
      const frame = m.advance((t += 16));
      if (frame.atEnd) break;
      const after = m.state;
      lineSteps.push(after.y - before.y);
      viewSteps.push(after.scrollTop - before.scrollTop);
      passed.add(frame.cursor);
      before = after;
    }
    // Over rows of 30px and of 2,000px alike.
    expect(passed.size).toBeGreaterThan(4);
    for (const step of lineSteps) expect(step).toBeCloseTo(speed * 16, 6);
    for (const step of viewSteps) expect(step).toBeCloseTo(speed * 16, 1);
  });

  it("gathers from rest over the ramp, with no jump on the first frame", () => {
    const rows = stack(MIXED);
    const speed = holdSpeed(LINE, DEFAULT_REPEAT_MS);
    // The cursor on the 2,000px row, which crosses the reading line: the line starts on it.
    const m = motion({ rows, cursor: 2, scrollTop: 0, direction: 1 });
    const start = m.state.scrollTop;
    m.advance(16);
    expect(m.state.scrollTop - start).toBeLessThan(speed * 16 * (16 / HOLD_RAMP_MS) + 0.5);
    let previous = m.state.scrollTop;
    let lastStep = 0;
    for (let t = 32; t <= HOLD_RAMP_MS + 32; t += 16) {
      m.advance(t);
      const step = m.state.scrollTop - previous;
      expect(step).toBeGreaterThanOrEqual(lastStep - 0.1);
      lastStep = step;
      previous = m.state.scrollTop;
    }
    expect(lastStep).toBeCloseTo(speed * 16, 1);
  });

  it("does not depend on the frame rate: the same time covers the same distance", () => {
    const rows = stack(MIXED);
    const a = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    const b = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    for (let t = 8; t <= 960; t += 8) a.advance(t);
    for (let t = 32; t <= 960; t += 32) b.advance(t);
    expect(a.state.y).toBeCloseTo(b.state.y, 6);
  });
});

describe("a held j: the cursor follows the line", () => {
  it("is, at every frame, the row under the cursor line, and that line is on screen near the reading line", () => {
    const rows = stack(MIXED);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    for (let t = 16; t < 4000; t += 16) {
      const frame = m.advance(t);
      const { y, scrollTop } = m.state;
      expect(rows[frame.cursor].top).toBeLessThanOrEqual(y);
      expect(rows[frame.cursor].bottom).toBeGreaterThan(y);
      expect(y).toBeGreaterThanOrEqual(scrollTop);
      expect(y).toBeLessThan(scrollTop + HEIGHT);
      if (frame.atEnd) break;
    }
  });

  it("settles the line on the reading line, a third of the way down", () => {
    const rows = stack(LONG);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    for (let t = 16; t < 3000; t += 16) m.advance(t);
    const { y, scrollTop } = m.state;
    expect(y - scrollTop).toBeCloseTo(HEIGHT * READING_LINE, 0);
  });

  it("starting on a row near the bottom, the view never moves backwards and the line drifts up to its place", () => {
    const rows = stack(LONG);
    // Cursor on row 3 (30px at 2100), shown near the bottom margin of a view at 1600.
    const m = motion({ rows, cursor: 3, scrollTop: 1560, direction: 1 });
    let previous = 1560;
    for (let t = 16; t < 3000; t += 16) {
      m.advance(t);
      expect(m.state.scrollTop).toBeGreaterThanOrEqual(previous);
      previous = m.state.scrollTop;
    }
    expect(m.state.y - m.state.scrollTop).toBeCloseTo(HEIGHT * READING_LINE, -1);
  });
});

describe("a held k is the mirror", () => {
  it("moves up at the same steady speed, the line two thirds of the way down", () => {
    const rows = stack(LONG);
    const speed = holdSpeed(LINE, DEFAULT_REPEAT_MS);
    const end = total(rows) - HEIGHT;
    const m = motion({ rows, cursor: rows.length - 1, scrollTop: end, direction: -1 });
    let t = 0;
    while (t < 2500) m.advance((t += 16));
    let before = m.state;
    for (let i = 0; i < 60; i++) {
      m.advance((t += 16));
      expect(before.y - m.state.y).toBeCloseTo(speed * 16, 6);
      expect(before.scrollTop - m.state.scrollTop).toBeCloseTo(speed * 16, 1);
      before = m.state;
    }
    expect(m.state.y - m.state.scrollTop).toBeCloseTo(HEIGHT * (1 - READING_LINE), 0);
  });
});

describe("the ends", () => {
  it("j stops on the last row's end, with the view at the end of the list", () => {
    const rows = stack([40, 60, 300, 30, 900, 45]);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    let frame = m.advance(16);
    let t = 16;
    while (!frame.atEnd && t < 20000) frame = m.advance((t += 16));
    expect(frame.atEnd).toBe(true);
    expect(frame.cursor).toBe(rows.length - 1);
    expect(frame.scrollTop).toBe(total(rows) - HEIGHT);
  });

  it("k stops on the first row's top, with the view at the top", () => {
    const rows = stack([40, 60, 300, 30, 900, 45]);
    const m = motion({ rows, cursor: rows.length - 1, scrollTop: total(rows) - HEIGHT, direction: -1 });
    let frame = m.advance(16);
    let t = 16;
    while (!frame.atEnd && t < 20000) frame = m.advance((t += 16));
    expect(frame.cursor).toBe(0);
    expect(frame.scrollTop).toBe(0);
  });

  it("has nothing to do on the last row with the view at the end", () => {
    const rows = stack([40, 60, 300]);
    const m = new HoldMotion({
      direction: 1,
      rows,
      cursor: 2,
      view: { scrollTop: 0, height: HEIGHT, maxScrollTop: 0 },
      line: LINE,
      speed: holdSpeed(LINE, DEFAULT_REPEAT_MS),
      now: 0,
    });
    expect(m.canMove()).toBe(false);
  });
});

describe("the release", () => {
  it("keeps the cursor on the row under the line and stops the view where a press would have put that row", () => {
    const rows = stack(MIXED);
    for (const stopAt of [500, 900, 1300, 1700, 2100, 2600, 3300]) {
      const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
      let t = 0;
      while (t < stopAt) m.advance((t += 16));
      const { scrollTop } = m.state;
      const before = m.advance(t).cursor;
      const settle = m.release();
      expect(settle.cursor).toBe(before);
      // Never back against the direction it was going, and only a little further on.
      expect(settle.target).toBeGreaterThanOrEqual(Math.round(scrollTop) - 1);
      // Where it rests the row is where `decideLanding` keeps it: nothing left to move.
      const again = decideLanding({
        direction: 1,
        view: { scrollTop: settle.target, height: HEIGHT, maxScrollTop: total(rows) - HEIGHT },
        row: rows[settle.cursor],
        line: LINE,
      });
      const fits = rows[settle.cursor].bottom - rows[settle.cursor].top <= HEIGHT - 4 * LINE;
      if (fits) expect(again.how).toBe("stay");
      else expect(again.scrollTop).toBeLessThanOrEqual(settle.target);
      expect(settle.velocity).toBeGreaterThan(0);
    }
  });

  it("coasts only a little: a third of the speed times a short stop", () => {
    const rows = stack(MIXED);
    const speed = holdSpeed(LINE, DEFAULT_REPEAT_MS);
    const m = motion({ rows, cursor: 2, scrollTop: 0, direction: 1 });
    let t = 0;
    // In the middle of the 2,000px row: a press there would leave the view where it is.
    while (m.state.y < 900) m.advance((t += 16));
    const at = m.state.scrollTop;
    const settle = m.release();
    expect(settle.cursor).toBe(2);
    expect(settle.target - at).toBeLessThanOrEqual((speed * 120) / 3 + 1);
    expect(settle.target - at).toBeGreaterThan(0);
  });

  it("released before the view moved, stays put unless the row asks for a landing", () => {
    const rows = stack(MIXED);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    m.advance(16);
    const settle = m.release();
    expect(settle.target).toBeLessThanOrEqual(2);
  });
});

describe("a change under the glide", () => {
  it("adopts a scroll somebody else made and goes on from there", () => {
    const rows = stack(MIXED);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    for (let t = 16; t < 500; t += 16) m.advance(t);
    m.adopt(1000);
    m.advance(516);
    expect(m.state.scrollTop).toBeGreaterThanOrEqual(1000);
  });

  it("keeps the line in the cursor's row when rows change height above it", () => {
    const rows = stack(MIXED);
    const m = motion({ rows, cursor: 2, scrollTop: 0, direction: 1 });
    for (let t = 16; t < 300; t += 16) m.advance(t);
    const cursor = m.advance(300).cursor;
    const inRow = m.state.y - rows[cursor].top;
    const taller = stack([140, ...MIXED.slice(1)]);
    m.rebase(taller, { scrollTop: m.state.scrollTop + 100, height: HEIGHT, maxScrollTop: total(taller) - HEIGHT });
    expect(m.state.y - taller[cursor].top).toBeCloseTo(inRow, 6);
  });
});

describe("a late frame", () => {
  it("moves the glide by at most two frames' worth, never a leap to make up the time lost", () => {
    const rows = stack(LONG);
    const speed = holdSpeed(LINE, DEFAULT_REPEAT_MS);
    const m = motion({ rows, cursor: 0, scrollTop: 0, direction: 1 });
    let t = 0;
    while (t < 2500) m.advance((t += 16));
    const before = m.state;
    m.advance((t += 150));
    expect(m.state.y - before.y).toBeCloseTo(speed * 34, 6);
    expect(m.state.scrollTop - before.scrollTop).toBeLessThanOrEqual(speed * 34 + 1);
  });
});
