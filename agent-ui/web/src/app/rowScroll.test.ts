// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { clampCursorToView, revealMatchInRow, revealRow, scrollVisibleRowBox, stepWithinRow } from "./rowScroll";
import { PROMPT_HEADER_CLASS } from "../promptHeader";

/* The DOM half of moving through the conversation, with the pinned prompt header shown over the list's top
   (`../promptHeader`): a 400px list at the viewport's top, rows at fixed content positions following `scrollTop`,
   a 20px text line (a 40px margin, a 60px step) and a 24px header. jsdom lays nothing out, so every rect here is
   planted; the pure decisions are pinned in `../jkScroll.test.ts`. */

const HEADER = 24;

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
  // jsdom's Range has no layout, hence no rect of its own; the tests below plant one.
  delete (Range.prototype as { getBoundingClientRect?: unknown }).getBoundingClientRect;
});

function layout(spans: [number, number][], header: boolean) {
  const scroller = document.createElement("div");
  const list = document.createElement("div");
  list.className = "message-list";
  scroller.append(list);
  let scrollTop = 1000;
  Object.defineProperty(list, "clientHeight", { configurable: true, value: 400 });
  Object.defineProperty(list, "scrollHeight", { configurable: true, value: 6000 });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(5600, v));
    },
  });
  list.getBoundingClientRect = () => ({ top: 0, bottom: 400 }) as DOMRect;
  const rows = spans.map(([top, bottom]) => {
    const row = document.createElement("div");
    row.dataset.navStop = "row";
    const body = document.createElement("div");
    body.className = "row-body";
    body.style.lineHeight = "20px";
    row.append(body);
    row.getBoundingClientRect = () => ({ top: top - scrollTop, bottom: bottom - scrollTop }) as DOMRect;
    list.append(row);
    return row;
  });
  if (header) {
    const el = document.createElement("div");
    el.className = PROMPT_HEADER_CLASS;
    el.getBoundingClientRect = () => ({ top: 0, bottom: HEADER }) as DOMRect;
    scroller.append(el);
  }
  document.body.append(scroller);
  return { list, rows, at: () => scrollTop };
}

describe("revealRow with the header shown", () => {
  it("lands a row below the header, with its margin", () => {
    const plain = layout([[1010, 1100]], false);
    revealRow(plain.list, plain.rows[0], 0);
    expect(plain.at()).toBe(970);
    document.body.innerHTML = "";
    const covered = layout([[1010, 1100]], true);
    revealRow(covered.list, covered.rows[0], 0);
    expect(covered.at()).toBe(1010 - HEADER - 40);
  });
});

describe("stepWithinRow with the header shown", () => {
  it("k steps a tall row until its head is clear of the header", () => {
    const plain = layout([[1050, 3000]], false);
    expect(stepWithinRow(plain.rows[0], -1, 1, false)).toBe(0);
    document.body.innerHTML = "";
    const covered = layout([[1050, 3000]], true);
    expect(stepWithinRow(covered.rows[0], -1, 1, false)).toBe(1);
    expect(covered.at()).toBe(1050 - 40 - HEADER);
  });
});

describe("clampCursorToView with the header shown", () => {
  it("re-homes a cursor whose row is wholly under the header to the first row below it", () => {
    // Row 0 shows 20px at the list's top, all of it under the 24px header.
    const plain = layout([[980, 1020], [1020, 1200]], false);
    expect(clampCursorToView(plain.list, plain.rows, 0)).toBeNull();
    document.body.innerHTML = "";
    const covered = layout([[980, 1020], [1020, 1200]], true);
    expect(clampCursorToView(covered.list, covered.rows, 0)).toBe(1);
    // A row that still shows below the header is left alone.
    expect(clampCursorToView(covered.list, covered.rows, 1)).toBeNull();
  });
});

describe("scrollVisibleRowBox with the header shown", () => {
  it("leaves a box wholly under the header to the conversation", () => {
    for (const header of [false, true]) {
      document.body.innerHTML = "";
      const { list, rows } = layout([[900, 1300]], header);
      const box = document.createElement("div");
      box.className = "tool-result-body";
      Object.defineProperty(box, "clientHeight", { configurable: true, value: 20 });
      Object.defineProperty(box, "scrollHeight", { configurable: true, value: 200 });
      box.getBoundingClientRect = () => ({ top: 2, bottom: 22 }) as DOMRect;
      rows[0].append(box);
      expect(list.contains(box)).toBe(true);
      expect(scrollVisibleRowBox(rows[0], 1)).toBe(!header);
    }
  });
});

describe("revealMatchInRow", () => {
  it("reveals a phrase spanning highlight nodes with folded offsets mapped to each node", () => {
    const { list, rows } = layout([[1000, 2000]], true);
    const body = rows[0].firstElementChild!;
    body.innerHTML = "<span>İlet</span> wanted";
    const seen: unknown[] = [];
    Object.defineProperty(Range.prototype, "getBoundingClientRect", {
      configurable: true,
      value(this: Range) {
        seen.push([this.startContainer, this.startOffset, this.endContainer, this.endOffset]);
        return { top: -100, bottom: -80, width: 80, height: 20 } as DOMRect;
      },
    });
    expect(revealMatchInRow(list, rows[0], "let wanted")).toBe(true);
    expect(seen).toEqual([[body.firstChild!.firstChild, 1, body.lastChild, 7]]);
    expect(list.scrollTop).toBe(876);
  });

  it.each([
    ["adjacent code blocks", "<pre>foo</pre><pre>bar</pre><p>a <em>foo</em><b>bar</b></p>"],
    ["list items", "<ul><li>foo</li><li>bar</li></ul><p>a <em>foo</em><b>bar</b></p>"],
    ["a line break", "foo<br>bar<p>a <em>foo</em><b>bar</b></p>"],
  ])("does not join text across blocks (%s), and reveals the real occurrence further down", (_name, html) => {
    const { list, rows } = layout([[1000, 2000]], true);
    const body = rows[0].firstElementChild!;
    body.innerHTML = html;
    const seen: Node[] = [];
    Object.defineProperty(Range.prototype, "getBoundingClientRect", {
      configurable: true,
      value(this: Range) {
        seen.push(this.startContainer);
        return { top: 450, bottom: 470, width: 60, height: 20 } as DOMRect;
      },
    });
    expect(revealMatchInRow(list, rows[0], "foobar")).toBe(true);
    // `toBe`, not `toEqual`: equal text nodes of two places compare equal.
    expect(seen).toHaveLength(1);
    expect(seen[0]).toBe(body.querySelector("em")!.firstChild);
  });

  it.each(["above", "below"])("reveals a match %s nested scroll boxes, inner first, then below the header", (side) => {
    const { list, rows } = layout([[1000, 2000]], true);
    const outer = document.createElement("div");
    const inner = document.createElement("div");
    outer.style.overflowY = "scroll";
    inner.style.overflowY = "auto";
    inner.className = "tool-result-body";
    inner.textContent = "needle";
    rows[0].append(outer);
    outer.append(inner);
    for (const box of [outer, inner]) {
      Object.defineProperties(box, { clientHeight: { value: 100 }, scrollHeight: { value: 1000 } });
      box.scrollTop = side === "above" ? 800 : 0;
    }
    outer.getBoundingClientRect = () => ({ top: 980 - list.scrollTop, bottom: 1080 - list.scrollTop }) as DOMRect;
    inner.getBoundingClientRect = () => ({ top: 1580 - list.scrollTop - outer.scrollTop,
      bottom: 1680 - list.scrollTop - outer.scrollTop }) as DOMRect;
    Object.defineProperty(Range.prototype, "getBoundingClientRect", {
      configurable: true,
      value() {
        const top = inner.getBoundingClientRect().top + 600 - inner.scrollTop;
        return { top, bottom: top + 20, width: 60, height: 20 } as DOMRect;
      },
    });
    const order: HTMLElement[] = [];
    for (const box of [inner, outer]) {
      let top = box.scrollTop;
      Object.defineProperty(box, "scrollTop", { get: () => top, set: (value: number) => { order.push(box); top = value; } });
    }
    expect(revealMatchInRow(list, rows[0], "needle")).toBe(true);
    expect(order).toEqual([inner, outer]);
    expect(inner.scrollTop).toBe(side === "above" ? 600 : 520);
    expect(outer.scrollTop).toBe(600);
    const range = document.createRange();
    const rect = range.getBoundingClientRect();
    expect(rect.top).toBeGreaterThanOrEqual(HEADER);
    expect(rect.bottom).toBeLessThanOrEqual(400);
    expect(rect.top).toBeGreaterThanOrEqual(inner.getBoundingClientRect().top);
    expect(rect.bottom).toBeLessThanOrEqual(inner.getBoundingClientRect().bottom);
    expect(rect.top).toBeGreaterThanOrEqual(outer.getBoundingClientRect().top);
    expect(rect.bottom).toBeLessThanOrEqual(outer.getBoundingClientRect().bottom);
  });

  it("skips an undrawn match and reveals a later drawn one", () => {
    const { list, rows } = layout([[1000, 2000]], false);
    rows[0].innerHTML = "<span hidden>needle</span><span>needle</span>";
    Object.defineProperty(Range.prototype, "getBoundingClientRect", {
      configurable: true,
      value(this: Range) {
        return this.startContainer.parentElement!.hidden
          ? { top: 0, bottom: 0, width: 0, height: 0 } as DOMRect
          : { top: 450, bottom: 470, width: 60, height: 20 } as DOMRect;
      },
    });
    expect(revealMatchInRow(list, rows[0], "needle")).toBe(true);
    expect(list.scrollTop).toBe(1070);
  });

  /** A one-text-node row; the range's rect is planted below the view so a found hit scrolls. Returns what the range
   *  was set to and whether the reveal moved the list. */
  function reveal(text: string, query: string) {
    const { list, rows } = layout([[1000, 1100]], false);
    rows[0].firstElementChild!.textContent = text;
    const seen: [number, number][] = [];
    Object.defineProperty(Range.prototype, "getBoundingClientRect", {
      configurable: true,
      value(this: Range) {
        seen.push([this.startOffset, this.endOffset]);
        return { top: 450, bottom: 470, width: 20, height: 20 } as DOMRect;
      },
    });
    const moved = revealMatchInRow(list, rows[0], query);
    return { seen, moved };
  }

  it("finds a hit by the offsets of the text as drawn when case folding changes its length", () => {
    // "İ" lowercases to two code units: a hit found in the lowered copy sat at offset 4 of a 3-unit node.
    expect(reveal("İİx", "x")).toEqual({ seen: [[2, 3]], moved: true });
    // A hit inside what one letter folded to takes that letter whole.
    expect(reveal("aİb", "i").seen).toEqual([[1, 2]]);
  });

  it("matches in any case, an exact one when the query has a capital", () => {
    expect(reveal("aẞb", "ß").seen).toEqual([[1, 2]]);
    expect(reveal("Hello hello", "hello").seen).toEqual([[0, 5]]);
    expect(reveal("Hello hello", "Hello").seen).toEqual([[0, 5]]);
    expect(reveal("hello Hello", "Hello").seen).toEqual([[6, 11]]);
  });

  it("counts a code point outside the BMP as the two units it is", () => {
    expect(reveal("😀 Hello", "hello").seen).toEqual([[3, 8]]);
    expect(reveal("😀İx", "x").seen).toEqual([[3, 4]]);
  });

  it("moves nothing and throws nothing when the range cannot be made", () => {
    const { list, rows } = layout([[1000, 1100]], false);
    rows[0].firstElementChild!.textContent = "İİx";
    vi.spyOn(Range.prototype, "setStart").mockImplementation(() => {
      throw new DOMException("out of range", "IndexSizeError");
    });
    expect(revealMatchInRow(list, rows[0], "x")).toBe(false);
    expect(list.scrollTop).toBe(1000);
  });

  it("has nothing to do with an empty query or no match", () => {
    expect(reveal("abc", "").moved).toBe(false);
    expect(reveal("abc", "z")).toEqual({ seen: [], moved: false });
  });
});
