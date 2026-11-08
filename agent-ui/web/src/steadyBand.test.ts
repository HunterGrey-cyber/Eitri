import { describe, expect, it } from "vitest";
import { FIGURE_INTERVAL_MS, UNREAD_SHOW_MS, initialSteady, steadyFacts, type SteadyMemo } from "./steadyBand";
import type { BandFacts } from "./band";

const base: BandFacts = { mode: "browse", pill: "⏵⏵ manual", showcmd: null, message: null, prompt: null, warn: null,
  unread: null, cards: 0, queued: 0, context: null, position: "1/1", model: "opus-5-5", usage: null, streaming: true };

/** Feeds `facts` at the given times, carrying the memo the way `StatusBand` does, and returns what was drawn each time. */
function feed(steps: [number, Partial<BandFacts>][], memo: SteadyMemo = initialSteady()) {
  const drawn: BandFacts[] = [];
  const out = { memo, due: null as number | null, reserve: {} as Record<string, number> };
  for (const [now, change] of steps) {
    const r = steadyFacts(out.memo, { ...base, ...change }, now);
    out.memo = r.memo;
    out.due = r.due;
    out.reserve = r.reserve;
    drawn.push(r.facts);
  }
  return { drawn, ...out };
}

describe("the band's figures while a turn streams", () => {
  it("the position changes at most once a second while items arrive, and lands on the newest value", () => {
    const steps: [number, Partial<BandFacts>][] = [];
    for (let i = 0; i < 30; i++) steps.push([10_000 + i * 100, { position: `${i + 1}/${i + 1}` }]);
    const { drawn, due } = feed(steps);
    const shown = drawn.map((f) => f.position);
    const changes = shown.filter((p, i) => i > 0 && p !== shown[i - 1]).length;
    // 3 s of items every 100 ms: about three redraws, not 29.
    expect(changes).toBeLessThanOrEqual(3);
    // The newest value is still owed: a timer is due so it lands without another item.
    expect(shown[shown.length - 1]).not.toBe("30/30");
    expect(due).not.toBeNull();
    const after = feed([[due!, { position: "30/30" }]], feed(steps).memo);
    expect(after.drawn[0].position).toBe("30/30");
  });

  it("a cursor move right after a key is drawn at once even mid-turn, with the count as it is", () => {
    const { drawn } = feed([
      [10_000, { position: "5/9" }],
      [10_100, { position: "5/11" }],
      [10_200, { position: "4/11", keyAge: 20 }],
      [10_300, { position: "3/12", keyAge: 15 }],
    ]);
    expect(drawn.map((f) => f.position)).toEqual(["5/9", "5/9", "4/11", "3/12"]);
  });

  it("the reader's keys never hold the other figures back: a move draws every figure as it is", () => {
    const u = (n: number) => ({ text: `${n}k tok`, title: "t" });
    const steps: [number, Partial<BandFacts>][] = [[10_000, { position: "5/9", usage: u(1) }]];
    for (let i = 1; i <= 20; i++) steps.push([10_000 + i * 200, { position: `${5 + (i % 2)}/9`, usage: u(1 + i), unread: "↓ ⚑", keyAge: 10 }]);
    const { drawn } = feed(steps);
    const last = drawn[drawn.length - 1];
    expect(last.usage?.text).toBe("21k tok");
    expect(last.unread).toBe("↓ ⚑");
  });

  it("the cursor following the newest row in a render of its own is not taken for the reader's move", () => {
    // An item lands (the count grows), then the cursor follows it a render later with no key pressed.
    const { drawn } = feed([
      [10_000, { position: "10/14" }],
      [10_100, { position: "10/15" }],
      [10_110, { position: "11/15" }],
      [10_400, { position: "11/16" }],
      [10_410, { position: "12/16" }],
    ]);
    expect(drawn.map((f) => f.position)).toEqual(["10/14", "10/14", "10/14", "10/14", "10/14"]);
  });

  it("keys typed into the composer are not cursor moves", () => {
    const { drawn } = feed([
      [10_000, { mode: "input", position: "10/14" }],
      [10_100, { mode: "input", position: "11/15", keyAge: 10 }],
    ]);
    expect(drawn.map((f) => f.position)).toEqual(["10/14", "10/14"]);
  });

  it("with no turn running every figure is drawn at once", () => {
    const { drawn } = feed([
      [10_000, { streaming: false, position: "1/3" }],
      [10_050, { streaming: false, position: "2/4" }],
      [10_100, { streaming: false, position: "3/5", usage: { text: "1k tok $0.01", title: "t" } }],
    ]);
    expect(drawn.map((f) => f.position)).toEqual(["1/3", "2/4", "3/5"]);
    expect(drawn[2].usage?.text).toBe("1k tok $0.01");
  });

  it("the usage figure follows the same once-a-second rule", () => {
    const u = (n: number) => ({ text: `${n}k tok $0.0${n}`, title: "t" });
    const { drawn } = feed([
      [10_000, { usage: u(1) }],
      [10_300, { usage: u(2) }],
      [10_600, { usage: u(3) }],
      [11_000, { usage: u(4) }],
    ]);
    expect(drawn.map((f) => f.usage?.text)).toEqual(["1k tok $0.01", "1k tok $0.01", "1k tok $0.01", "4k tok $0.04"]);
  });

  it("the turn's end draws the newest figures at once", () => {
    const { drawn } = feed([
      [10_000, { position: "1/1" }],
      [10_100, { position: "2/2" }],
      [10_200, { streaming: false, position: "3/3" }],
    ]);
    expect(drawn.map((f) => f.position)).toEqual(["1/1", "1/1", "3/3"]);
  });
});

describe("the reserved widths", () => {
  it("the position reserves the width of M/M, so N growing a digit moves nothing", () => {
    const { reserve } = feed([[10_000, { position: "9/120" }]]);
    expect(reserve.position).toBe("120/120".length);
  });

  it("a figure's width only grows while a turn runs, and is let go when it ends", () => {
    const u = (text: string) => ({ text, title: "t" });
    const run = feed([
      [10_000, { usage: u("12.3k tok $0.10") }],
      [12_000, { usage: u("13k tok $0.11") }],
    ]);
    expect(run.reserve.usage).toBe("12.3k tok $0.10".length);
    const ended = feed([[13_000, { streaming: false, usage: u("13k tok $0.11") }]], run.memo);
    expect(ended.reserve.usage).toBeUndefined();
  });
});

describe("↓N never blinks", () => {
  it("appears only after the unseen rows have been there half a second", () => {
    const { drawn, due } = feed([
      [10_000, { unread: "↓1" }],
      [10_200, { unread: "↓2" }],
    ]);
    expect(drawn.map((f) => f.unread)).toEqual([null, null]);
    expect(due).toBe(10_000 + UNREAD_SHOW_MS);
  });

  it("a flicker of unseen state shorter than that never draws it", () => {
    const { drawn } = feed([
      [10_000, { unread: "↓1" }],
      [10_100, { unread: null }],
      [10_200, { unread: "↓1" }],
      [10_300, { unread: null }],
      [10_700, { unread: null }],
    ]);
    expect(drawn.every((f) => f.unread === null)).toBe(true);
  });

  it("once up it stays, its count throttled, until the reader reaches the end", () => {
    const { drawn } = feed([
      [10_000, { unread: "↓1" }],
      [10_500, { unread: "↓2" }],
      [10_700, { unread: "↓3" }],
      [11_000, { unread: "↓4" }],
      [11_600, { unread: "↓5" }],
      [11_700, { unread: null }],
    ]);
    expect(drawn.map((f) => f.unread)).toEqual([null, "↓2", "↓2", "↓2", "↓5", null]);
  });

  it("its width is reserved for the widest count this turn", () => {
    const { reserve } = feed([
      [10_000, { unread: "↓9" }],
      [10_600, { unread: "↓9" }],
      [11_700, { unread: "↓10" }],
    ]);
    expect(reserve.unread).toBe(3);
  });

  it("it keeps room for two digits from the start, so ↓9 to ↓10 moves nothing", () => {
    const { reserve } = feed([
      [10_000, { unread: "↓1" }],
      [10_600, { unread: "↓1" }],
    ]);
    expect(reserve.unread).toBe(3);
  });

  it("the interval between figure redraws is about a second", () => {
    expect(FIGURE_INTERVAL_MS).toBeGreaterThanOrEqual(900);
    expect(FIGURE_INTERVAL_MS).toBeLessThanOrEqual(1200);
  });
});
