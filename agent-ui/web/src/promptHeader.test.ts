// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import {
  effectiveTop,
  firstRowBelow,
  HEADER_TEXT_MAX,
  headerBand,
  headerCount,
  headerInset,
  headerPrompt,
  headerText,
  landingInset,
  PROMPT_HEADER_CLASS,
  promptHeaderOver,
} from "./promptHeader";
import type { TimelineItem } from "./timeline";

const p = (seq: number, delivered = false): TimelineItem =>
  ({ kind: "prompt", seq, key: `u-${seq}`, text: `prompt ${seq}`, ...(delivered ? { delivered: true } : {}) }) as TimelineItem;
const m = (seq: number, depth?: number): TimelineItem =>
  ({ kind: "message", seq, key: `m-${seq}`, text: `reply ${seq}`, ...(depth ? { depth } : {}) }) as TimelineItem;
const w = (seq: number): TimelineItem =>
  ({ kind: "wake", seq, key: `w-${seq}`, text: "a task finished", body: null, scheduled: false, count: 1 }) as TimelineItem;

describe("headerPrompt: which prompt the header names", () => {
  const timeline = [p(0), m(1), m(2), p(3), m(4), m(5)];

  it("names the nearest prompt above the top row, with its place among the prompts", () => {
    expect(headerPrompt(timeline, 2, false)).toEqual({ index: 0, ordinal: 1, total: 2 });
    expect(headerPrompt(timeline, 5, false)).toEqual({ index: 3, ordinal: 2, total: 2 });
  });

  it("names nothing while the view follows the newest row", () => {
    expect(headerPrompt(timeline, 5, true)).toBeNull();
  });

  it("names nothing while the prompt's own row is the top row (it is on screen)", () => {
    expect(headerPrompt(timeline, 3, false)).toBeNull();
    expect(headerPrompt(timeline, 0, false)).toBeNull();
  });

  it("names nothing above the first prompt", () => {
    expect(headerPrompt([m(0), m(1), p(2), m(3)], 1, false)).toBeNull();
  });

  it("names nothing with no row on screen", () => {
    expect(headerPrompt(timeline, -1, false)).toBeNull();
    expect(headerPrompt(timeline, timeline.length, false)).toBeNull();
    expect(headerPrompt([], 0, false)).toBeNull();
  });

  it("passes over a delivered prompt, which neither opens a turn nor counts", () => {
    const withDelivered = [p(0), m(1), p(2, true), m(3), p(4), m(5)];
    expect(headerPrompt(withDelivered, 3, false)).toEqual({ index: 0, ordinal: 1, total: 2 });
    // A delivered prompt as the top row is passed over too.
    expect(headerPrompt(withDelivered, 2, false)).toEqual({ index: 0, ordinal: 1, total: 2 });
    expect(headerPrompt(withDelivered, 5, false)).toEqual({ index: 4, ordinal: 2, total: 2 });
  });

  it("names nothing when a wake lies between the prompt and the top row, the top row included", () => {
    const withWake = [p(0), m(1), w(2), m(3)];
    expect(headerPrompt(withWake, 3, false)).toBeNull();
    expect(headerPrompt(withWake, 2, false)).toBeNull();
    expect(headerPrompt(withWake, 1, false)).toEqual({ index: 0, ordinal: 1, total: 1 });
    // A wake above the prompt is another turn's: it changes nothing below the prompt.
    expect(headerPrompt([w(0), p(1), m(2)], 2, false)).toEqual({ index: 1, ordinal: 1, total: 1 });
  });

  it("reads a delegation's nested rows as part of the turn they sit in", () => {
    expect(headerPrompt([p(0), m(1), m(2, 1), m(3, 1)], 3, false)).toEqual({ index: 0, ordinal: 1, total: 1 });
  });

  it("counts every prompt that opens a turn, wherever the reader is", () => {
    const nine = Array.from({ length: 9 }, (_, i) => [p(2 * i), m(2 * i + 1)]).flat();
    expect(headerPrompt(nine, 7, false)).toEqual({ index: 6, ordinal: 4, total: 9 });
    expect(headerCount({ index: 6, ordinal: 4, total: 9 })).toBe("prompt 4/9");
  });
});

describe("headerText: the prompt as one line", () => {
  it("makes every run of white space, line breaks included, one space", () => {
    expect(headerText("  fix this:\n\n\tthe   parser\r\n")).toBe("fix this: the parser");
  });

  it("keeps no more than HEADER_TEXT_MAX characters, and never half a surrogate pair", () => {
    expect(headerText("x".repeat(HEADER_TEXT_MAX + 50))).toHaveLength(HEADER_TEXT_MAX);
    const emoji = "x".repeat(HEADER_TEXT_MAX - 1) + "😀" + "tail";
    const cut = headerText(emoji);
    expect(cut).toBe("x".repeat(HEADER_TEXT_MAX - 1));
    expect(/[\uD800-\uDBFF]$/.test(cut)).toBe(false);
  });
});

describe("effectiveTop: the top of what shows", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  function scroller(withHeader: boolean, headerBottom = 124) {
    const parent = document.createElement("div");
    const list = document.createElement("div");
    list.className = "message-list";
    list.getBoundingClientRect = () => ({ top: 100, bottom: 500 }) as DOMRect;
    parent.append(list);
    if (withHeader) {
      const header = document.createElement("div");
      header.className = PROMPT_HEADER_CLASS;
      header.getBoundingClientRect = () => ({ top: 100, bottom: headerBottom }) as DOMRect;
      parent.append(header);
    }
    document.body.append(parent);
    return list;
  }

  it("is the list's own top without a header", () => {
    const list = scroller(false);
    expect(promptHeaderOver(list)).toBeNull();
    expect(headerInset(list)).toBe(0);
    expect(effectiveTop(list)).toBe(100);
  });

  it("is below the header while one is shown", () => {
    const list = scroller(true);
    expect(promptHeaderOver(list)?.className).toBe(PROMPT_HEADER_CLASS);
    expect(headerInset(list)).toBe(24);
    expect(effectiveTop(list)).toBe(124);
  });

  it("never covers more than the list, nor less than nothing", () => {
    expect(headerInset(scroller(true, 900))).toBe(400);
    document.body.innerHTML = "";
    expect(headerInset(scroller(true, 0))).toBe(0);
  });

  it("measures through the geometry it is given", () => {
    const list = scroller(true);
    const rect = (el: Element) => (el === list ? { top: 0, bottom: 300 } : { top: 0, bottom: 30 });
    expect(effectiveTop(list, rect)).toBe(30);
  });
});

/* The header's band and the landing a key picks for the header it will have: a fixed height whether or not the header
   is drawn at the moment, so showing it cannot change which row it is decided for. */
describe("headerBand, firstRowBelow and landingInset", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  function listWithRows(spans: Array<[number, number]>, headerBottom: number | null = null) {
    const parent = document.createElement("div");
    const list = document.createElement("div");
    list.getBoundingClientRect = () => ({ top: 0, bottom: 300 }) as DOMRect;
    const rows = spans.map(([top, bottom]) => {
      const row = document.createElement("div");
      row.getBoundingClientRect = () => ({ top, bottom }) as DOMRect;
      list.append(row);
      return row;
    });
    parent.append(list);
    if (headerBottom !== null) {
      const header = document.createElement("div");
      header.className = PROMPT_HEADER_CLASS;
      header.getBoundingClientRect = () => ({ top: 0, bottom: headerBottom }) as DOMRect;
      parent.append(header);
    }
    document.body.append(parent);
    return { list, rows };
  }

  it("is the shown header's height, else the editor row it takes when drawn, else 24px", () => {
    expect(headerBand(listWithRows([], 30).list)).toBe(30);
    const { list } = listWithRows([]);
    expect(headerBand(list)).toBe(24);
    list.style.setProperty("--nv-editor-row", "18px");
    expect(headerBand(list)).toBe(18);
  });

  it("finds the first row whose bottom is past the top and whose top is above the bottom", () => {
    const { rows } = listWithRows([[-90, 10], [10, 60], [60, 160]]);
    const rect = (el: Element) => el.getBoundingClientRect();
    expect(firstRowBelow(rows, 24, 300, rect)).toBe(1);
    expect(firstRowBelow(rows, 0, 300, rect)).toBe(0);
    expect(firstRowBelow(rows, 24, 300, rect, -50)).toBe(2); // scrolled 50px further
    expect(firstRowBelow(rows, 24, 50, rect, 200)).toBe(-1); // pushed wholly below the bottom
  });

  // Prompt 0, replies 1 and 2, prompt 3, replies 4 and 5: 100px rows, the list 300px tall.
  const timeline = [p(0), m(1), m(2), p(3), m(4), m(5)];
  const stack = () => listWithRows(timeline.map((_, i) => [i * 100, i * 100 + 100] as [number, number]));
  const rect = (el: Element) => el.getBoundingClientRect();

  it("lands a reply below the header its landing brings, though none is drawn now", () => {
    const { list, rows } = stack();
    // The reply at 400..500 wanted at the list's top: the first row past the band is that reply, under prompt 3.
    expect(landingInset(list, rows, timeline, (inset) => 400 - inset, rect)).toBe(24);
  });

  it("keeps a prompt's own row at the list's top: nothing is named over it", () => {
    const { list, rows } = stack();
    expect(landingInset(list, rows, timeline, (inset) => 300 - inset, rect)).toBe(0);
  });

  it("keeps the top free where the rows at the top are a wake's or above the first prompt", () => {
    const wake = [p(0), m(1), w(2), m(3)];
    const { list, rows } = listWithRows(wake.map((_, i) => [i * 100, i * 100 + 100] as [number, number]));
    expect(landingInset(list, rows, wake, (inset) => 300 - inset, rect)).toBe(0); // the reply after the wake
    expect(landingInset(list, rows, wake, (inset) => 0 - inset, rect)).toBe(0); // the first prompt itself
  });

  it("leaves the whole list to the key when no header could be drawn", () => {
    const { list, rows } = stack();
    list.getBoundingClientRect = () => ({ top: 0, bottom: 0 }) as DOMRect;
    expect(landingInset(list, rows, timeline, (inset) => 400 - inset, rect)).toBe(0);
  });
});
