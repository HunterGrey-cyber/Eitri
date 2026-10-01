import { describe, expect, it } from "vitest";
import {
  boxStepTarget,
  decideLanding,
  decidePress,
  MARGIN_LINES,
  READING_LINE,
  STEP_LINES,
  type BoxState,
  type View,
} from "./jkScroll";

/* The decisions behind `j`/`k` in the panel, as pure functions over whole-pixel geometry: where the
   view should be after a press, and what the press did. No DOM, no layout -- the real engine's
   behaviour is the job of `shell/tests/panel_jk_scroll.rs`; these pin the arithmetic.

   Most cases use a 400px view and a 20px line: the margin is then 40px (two lines), a step 60px
   (three lines) and the reading line 133.33px (a third of the view), small enough to compute by
   hand. The realistic sizes (a 900px view, a 24.75px line, the sub-pixel lists WebKit reports) are
   in the groups further down. */

const LINE = 20;
const MARGIN = MARGIN_LINES * LINE; // 40
const STEP = STEP_LINES * LINE; // 60

const view = (scrollTop: number, height = 400, maxScrollTop = 100_000): View => ({ scrollTop, height, maxScrollTop });
const span = (top: number, bottom: number) => ({ top, bottom });

describe("the constants", () => {
  it("keep the margin at two lines, the step at three and the reading line at a third", () => {
    expect(MARGIN_LINES).toBe(2);
    expect(STEP_LINES).toBe(3);
    expect(READING_LINE).toBeCloseTo(1 / 3, 10);
  });
});

describe("decideLanding -- a row that is already visible does not move the view", () => {
  it.each([1, -1, 0] as const)("direction %i: a row inside the margins stays put", (direction) => {
    const landing = decideLanding({ direction, view: view(0), row: span(100, 180), line: LINE });
    expect(landing).toEqual({ scrollTop: 0, how: "stay" });
  });

  it("a row exactly on the margin line still counts as visible, with a pixel of slack either side", () => {
    // bottom margin line is at 400 - 40 = 360; top margin line at 40.
    expect(decideLanding({ direction: 1, view: view(0), row: span(280, 360), line: LINE }).how).toBe("stay");
    expect(decideLanding({ direction: 1, view: view(0), row: span(281, 361), line: LINE }).how).toBe("stay");
    expect(decideLanding({ direction: -1, view: view(0), row: span(40, 120), line: LINE }).how).toBe("stay");
    expect(decideLanding({ direction: -1, view: view(0), row: span(39, 119), line: LINE }).how).toBe("stay");
  });

  it("a tall row is never 'already visible': it cannot sit inside both margins", () => {
    const landing = decideLanding({ direction: 1, view: view(0), row: span(100, 1090), line: LINE });
    expect(landing.how).not.toBe("stay");
  });
});

describe("decideLanding -- a row that fits is brought in by the least scroll, keeping a two-line margin", () => {
  it("below the view: its bottom goes to the bottom margin line", () => {
    const landing = decideLanding({ direction: 1, view: view(0), row: span(330, 410), line: LINE });
    expect(landing).toEqual({ scrollTop: 410 - 400 + MARGIN, how: "nearest" }); // 50
  });

  it("a row wholly below the view, adjacent to what was just read, slides in by its own height plus the margin", () => {
    // the previous row ended on the bottom margin line (360); this one is 82px tall.
    const landing = decideLanding({ direction: 1, view: view(0), row: span(360, 442), line: LINE });
    expect(landing.scrollTop).toBe(442 - 400 + MARGIN);
  });

  it("above the view: its top goes to the top margin line", () => {
    const landing = decideLanding({ direction: -1, view: view(500), row: span(480, 560), line: LINE });
    expect(landing).toEqual({ scrollTop: 480 - MARGIN, how: "nearest" }); // 440
  });

  it("the direction of the press does not matter for a row that fits: a j into a row above scrolls up to it", () => {
    expect(decideLanding({ direction: 1, view: view(500), row: span(480, 560), line: LINE }).scrollTop).toBe(440);
    expect(decideLanding({ direction: -1, view: view(0), row: span(330, 410), line: LINE }).scrollTop).toBe(50);
    expect(decideLanding({ direction: 0, view: view(500), row: span(480, 560), line: LINE }).scrollTop).toBe(440);
  });

  it("clamps at the ends of the list: the first row cannot get its top margin, the last its bottom one", () => {
    expect(decideLanding({ direction: -1, view: view(100), row: span(12, 60), line: LINE }).scrollTop).toBe(0);
    expect(decideLanding({ direction: 1, view: view(560, 400, 600), row: span(940, 1000), line: LINE }).scrollTop).toBe(
      600,
    );
  });

  it("a row exactly as tall as the margins leave room for still fits", () => {
    const room = 400 - 2 * MARGIN; // 320
    const landing = decideLanding({ direction: 1, view: view(0), row: span(380, 380 + room), line: LINE });
    expect(landing.how).toBe("nearest");
    expect(landing.scrollTop).toBe(380 + room - 400 + MARGIN); // top margin line == row top
  });
});

describe("decideLanding -- j into a tall row puts its head on the reading line", () => {
  it("a tall row starting where the view ends: the head goes a third of the way down", () => {
    const landing = decideLanding({ direction: 1, view: view(0), row: span(400, 1390), line: LINE });
    expect(landing).toEqual({ scrollTop: Math.round(400 - 400 / 3), how: "head" }); // 267
  });

  it("the tail of what was just read stays in the top third: the view moves at most two thirds of itself", () => {
    // worst case: the row's head sits exactly on the view's bottom edge.
    const landing = decideLanding({ direction: 1, view: view(1000), row: span(1400, 3000), line: LINE });
    expect(landing.scrollTop - 1000).toBeLessThanOrEqual(Math.ceil((400 * 2) / 3));
  });

  it("never scrolls up for a j: a head already above the reading line stays where it is", () => {
    const landing = decideLanding({ direction: 1, view: view(0), row: span(50, 1040), line: LINE });
    expect(landing.scrollTop).toBe(0);
  });

  it("a head lower than the reading line is brought up to it", () => {
    const landing = decideLanding({ direction: 1, view: view(0), row: span(300, 1290), line: LINE });
    expect(landing.scrollTop).toBe(Math.round(300 - 400 / 3)); // 167
  });
});

describe("decideLanding -- k into a tall row enters at its end, on the line two thirds down", () => {
  it("the view had the row's tail just above its top: the tail lands two thirds of the way down", () => {
    const landing = decideLanding({ direction: -1, view: view(1390), row: span(400, 1390), line: LINE });
    expect(landing).toEqual({ scrollTop: Math.round(1390 - (400 * 2) / 3), how: "tail" }); // 1123
  });

  it("the row just left stays in the bottom third: the view moves at most two thirds of itself", () => {
    const landing = decideLanding({ direction: -1, view: view(1000), row: span(100, 1000), line: LINE });
    expect(1000 - landing.scrollTop).toBeLessThanOrEqual(Math.ceil((400 * 2) / 3));
  });

  it("never scrolls down for a k: a tail already lower than the line stays", () => {
    const landing = decideLanding({ direction: -1, view: view(300), row: span(0, 650), line: LINE });
    expect(landing.scrollTop).toBe(300);
  });

  it("is the mirror image of j: head at a third going down, tail at two thirds going up", () => {
    const down = decideLanding({ direction: 1, view: view(0), row: span(400, 1390), line: LINE });
    const up = decideLanding({ direction: -1, view: view(1390), row: span(400, 1390), line: LINE });
    expect(400 - down.scrollTop).toBeCloseTo(133, 0);
    expect(1390 - up.scrollTop).toBeCloseTo(267, 0);
  });
});

describe("decideLanding -- a cursor move that no j/k made (direction 0)", () => {
  it("a tall row below the view is entered at its head, one above at its tail", () => {
    expect(decideLanding({ direction: 0, view: view(0), row: span(500, 1500), line: LINE }).how).toBe("head");
    expect(decideLanding({ direction: 0, view: view(2000), row: span(500, 1500), line: LINE }).how).toBe("tail");
  });

  it("a tall row already partly on screen is left alone", () => {
    expect(decideLanding({ direction: 0, view: view(600), row: span(500, 1500), line: LINE })).toEqual({
      scrollTop: 600,
      how: "stay",
    });
  });
});

describe("decideLanding -- whole pixels, whatever the numbers", () => {
  it.each([
    [1, view(0, 400), span(400.4, 1390.2)],
    [-1, view(1390, 400), span(400.7, 1390.3)],
    [1, view(0, 900), span(900.391, 4000.2)],
    [1, view(12.5, 903), span(903.74, 983.9)],
  ] as const)("direction %i lands on an integer", (direction, v, row) => {
    const landing = decideLanding({ direction, view: v, row, line: 24.75 });
    expect(Number.isInteger(landing.scrollTop) || landing.how === "stay").toBe(true);
  });
});

describe("decideLanding -- realistic sizes (900px view, 24.75px line)", () => {
  const H = 900;
  const line = 24.75;

  it("j into a tall row right below a short one moves under two thirds of the view, at 1.0 and at 1.5", () => {
    // the short row ended on the bottom margin line (900 - 49.5), the tall one starts there.
    for (const height of [900, 904]) {
      const start = height - 49.5;
      const landing = decideLanding({ direction: 1, view: view(0, height), row: span(start, start + 1475), line });
      const moved = landing.scrollTop;
      expect(moved).toBeGreaterThan(0);
      expect(moved).toBeLessThanOrEqual(Math.ceil((height * 2) / 3));
      expect(landing.how).toBe("head");
    }
  });

  it("k into the tall row above moves under two thirds of the view, and ends with the tail two thirds down", () => {
    const landing = decideLanding({ direction: -1, view: view(2000, H), row: span(500, 2000), line });
    expect(2000 - landing.scrollTop).toBeLessThanOrEqual(600);
    expect(2000 - landing.scrollTop).toBe(Math.round((H * 2) / 3));
  });

  it("a short row sliding in moves exactly its own height plus the margin", () => {
    const landing = decideLanding({ direction: 1, view: view(0, H), row: span(850.5, 932.5), line });
    expect(landing.scrollTop).toBe(Math.round(932.5 - H + 49.5));
  });
});

const box = (over: Partial<BoxState> = {}): BoxState => ({
  top: 200,
  bottom: 460,
  scrollTop: 0,
  clientHeight: 260,
  scrollHeight: 1000,
  ...over,
});

describe("boxStepTarget -- a tool result's own box moves three of its lines per press", () => {
  it("j scrolls down by three lines and k back up", () => {
    expect(boxStepTarget(box(), 1, 18)).toBe(54);
    expect(boxStepTarget(box({ scrollTop: 200 }), -1, 18)).toBe(146);
  });

  it("stops at the ends: nothing to give at the start going up or at the end going down", () => {
    expect(boxStepTarget(box({ scrollTop: 0 }), -1, 18)).toBeNull();
    expect(boxStepTarget(box({ scrollTop: 740 }), 1, 18)).toBeNull();
    expect(boxStepTarget(box({ scrollTop: 739.5 }), 1, 18)).toBeNull(); // within the pixel of slack
  });

  it("the last step is whatever is left, never past the end", () => {
    expect(boxStepTarget(box({ scrollTop: 720 }), 1, 18)).toBe(740);
    expect(boxStepTarget(box({ scrollTop: 20 }), -1, 18)).toBe(0);
  });

  it("a box with nothing to scroll gives nothing either way", () => {
    expect(boxStepTarget(box({ scrollHeight: 260 }), 1, 18)).toBeNull();
    expect(boxStepTarget(box({ scrollHeight: 260 }), -1, 18)).toBeNull();
  });
});

describe("decidePress -- what one j/k does, in order: the box, then the row, then the move", () => {
  const common = { view: view(0), line: LINE, boxLine: 18 };

  it("scrolls the cursor row's box while it can move and is on screen", () => {
    const press = decidePress({ ...common, direction: 1, row: span(100, 441), box: box({ top: 160, bottom: 420 }) });
    expect(press).toEqual({ kind: "box", boxScrollTop: 54 });
  });

  it("skips a box that is wholly off screen", () => {
    const press = decidePress({ ...common, direction: 1, row: span(100, 941), box: box({ top: 600, bottom: 860 }) });
    expect(press.kind).toBe("row");
  });

  it("skips a box that cannot move that way", () => {
    const press = decidePress({
      ...common,
      direction: 1,
      row: span(100, 360),
      box: box({ top: 160, bottom: 360, scrollTop: 740 }),
    });
    expect(press.kind).toBe("move");
  });

  it("steps through a row taller than what is left of it, three lines at a time", () => {
    const press = decidePress({ ...common, direction: 1, row: span(100, 1090), box: null });
    expect(press).toEqual({ kind: "row", scrollTop: STEP });
  });

  it("a step never goes past what is left to reveal, and the last one leaves the end on the bottom margin line", () => {
    const press = decidePress({ ...common, view: view(700), direction: 1, row: span(100, 1090), box: null });
    expect(press).toEqual({ kind: "row", scrollTop: 1090 - 400 + MARGIN }); // 730, not 760
  });

  it("moves on once the row's end is on the margin line", () => {
    const press = decidePress({ ...common, view: view(730), direction: 1, row: span(100, 1090), box: null });
    expect(press).toEqual({ kind: "move" });
  });

  it("the sub-pixel rule: a row whose end is a pixel past the margin line is finished", () => {
    const press = decidePress({ ...common, view: view(729), direction: 1, row: span(100, 1090), box: null });
    expect(press).toEqual({ kind: "move" });
    const two = decidePress({ ...common, view: view(728), direction: 1, row: span(100, 1090), box: null });
    expect(two.kind).toBe("row");
  });

  it("k mirrors it: steps up through the row, the last step leaving its top on the top margin line", () => {
    const first = decidePress({ ...common, view: view(1000), direction: -1, row: span(100, 1090), box: null });
    expect(first).toEqual({ kind: "row", scrollTop: 1000 - STEP });
    const last = decidePress({ ...common, view: view(110), direction: -1, row: span(100, 1090), box: null });
    expect(last).toEqual({ kind: "row", scrollTop: 100 - MARGIN }); // 60, not 90
    const done = decidePress({ ...common, view: view(60), direction: -1, row: span(100, 1090), box: null });
    expect(done).toEqual({ kind: "move" });
  });

  it("a row inside its margins is a plain move", () => {
    const press = decidePress({ ...common, direction: 1, row: span(100, 180), box: null });
    expect(press).toEqual({ kind: "move" });
  });

  it("a press that could make no progress (the list's own end) falls through to a move", () => {
    const press = decidePress({ ...common, view: view(690, 400, 690), direction: 1, row: span(100, 1090), box: null });
    expect(press).toEqual({ kind: "move" });
    const top = decidePress({ ...common, view: view(0, 400, 690), direction: -1, row: span(12, 600), box: null });
    expect(top).toEqual({ kind: "move" });
  });

  it("a row wholly out of view is brought back first, and that press is its own", () => {
    const press = decidePress({ ...common, view: view(2000), direction: 1, row: span(100, 180), box: null });
    expect(press).toEqual({ kind: "reveal", scrollTop: 100 - MARGIN });
  });

  it("a wholly out-of-view row that cannot be brought any closer falls through to a move", () => {
    const press = decidePress({ ...common, view: view(0, 400, 0), direction: 1, row: span(900, 980), box: null });
    expect(press).toEqual({ kind: "move" });
  });

  it("the box is only offered while the row is on screen: an off-screen row reveals before it scrolls the box", () => {
    const press = decidePress({
      ...common,
      view: view(2000),
      direction: 1,
      row: span(100, 441),
      box: box({ top: 160, bottom: 420 }),
    });
    expect(press.kind).toBe("reveal");
  });

  it("a tall row wholly out of view comes back at its nearest end, whichever key asked", () => {
    // 341px does not fit a 400px view with two-line margins, so it is entered, not nudged: above the view
    // its tail is the near end, below the view its head is.
    const above = decidePress({ ...common, view: view(2000), direction: 1, row: span(100, 441), box: null });
    expect(above).toEqual({ kind: "reveal", scrollTop: Math.round(441 - (400 * 2) / 3) });
    const below = decidePress({ ...common, view: view(0), direction: -1, row: span(1500, 2491), box: null });
    expect(below).toEqual({ kind: "reveal", scrollTop: Math.round(1500 - 400 / 3) });
  });
});

describe("decidePress -- sub-pixel list heights and zoom", () => {
  it("a tall row whose top is a fraction below a 900.391px view is entered, not skipped (the dead press)", () => {
    // WebKit at zoom 1.0: the list's rect is 900.391px high, clientHeight 900; the geometry reader
    // hands the pure code whole pixels, and the row's top is at 900 -- the old `r.top < l.bottom` test
    // on the fractional rect read that as "already shown" and scrolled nothing.
    const landing = decideLanding({ direction: 1, view: view(0, 900), row: span(900, 2375), line: 24.75 });
    expect(landing.scrollTop).toBe(600);
  });

  it("the same transcript at zoom 1.5 (903.74px, clientHeight 904) takes the same branch, not a whole view", () => {
    const landing = decideLanding({ direction: 1, view: view(0, 904), row: span(904, 2355), line: 24.75 });
    expect(landing.how).toBe("head");
    expect(landing.scrollTop).toBe(Math.round(904 - 904 / 3)); // 603
  });

  it("k into a tall row bottom-aligned with the view's top moves two thirds, not a whole view (-901 of 900)", () => {
    const landing = decideLanding({ direction: -1, view: view(2016, 900), row: span(0, 2016), line: 24.75 });
    expect(2016 - landing.scrollTop).toBe(600);
  });
});

describe("a view too short for the margins (a squeezed panel)", () => {
  /* Two lines of margin is 49.5px at a 24.75px line, more than a 40px view holds. The margins are capped
     at a quarter of the view, so a row is never judged against margin lines that cross, and a count of
     presses over a row cannot step the view past it and reveal it again forever. */
  const LINE_PX = 24.75;

  it("caps the margin at a quarter of the view, so a row that fits still has room", () => {
    const landing = decideLanding({ direction: 1, view: view(0, 40), row: span(60, 70), line: LINE_PX });
    // a 10px row in a 40px view with a 10px margin: bottom margin line at 30, so the row's bottom (70) goes there
    expect(landing).toEqual({ scrollTop: 40, how: "nearest" });
  });

  it("walks a row taller than the view to its end and then moves on, never back", () => {
    let scrollTop = 100;
    const trace: string[] = [];
    for (let i = 0; i < 20; i++) {
      const press = decidePress({
        direction: 1,
        view: view(scrollTop, 40),
        row: span(100, 200),
        line: LINE_PX,
        box: null,
        boxLine: LINE_PX,
      });
      trace.push(press.kind);
      if (press.kind === "move") break;
      scrollTop = press.kind === "box" ? scrollTop : press.scrollTop;
    }
    expect(trace[trace.length - 1]).toBe("move");
    expect(trace.length).toBeLessThan(10);
    expect(trace).not.toContain("reveal");
  });

  it("k mirrors it", () => {
    let scrollTop = 160;
    const trace: string[] = [];
    for (let i = 0; i < 20; i++) {
      const press = decidePress({
        direction: -1,
        view: view(scrollTop, 40),
        row: span(100, 200),
        line: LINE_PX,
        box: null,
        boxLine: LINE_PX,
      });
      trace.push(press.kind);
      if (press.kind === "move") break;
      scrollTop = press.kind === "box" ? scrollTop : press.scrollTop;
    }
    expect(trace[trace.length - 1]).toBe("move");
    expect(trace.length).toBeLessThan(10);
    expect(trace).not.toContain("reveal");
  });
});
