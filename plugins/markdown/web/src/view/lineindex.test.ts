import { describe, expect, it, vi } from "vitest";
import { buildIndex, lastLineAtOrBefore, lastTopAtOrBefore, measure, type Measured } from "./lineindex";

const el = () => document.createElement("div");

function m(line: number, end: number | null, top: number, height: number, extra: Partial<Measured> = {}): Measured {
  return { el: el(), line, end, top, height, visible: true, hasTaggedVisibleDescendant: false, ...extra };
}

describe("buildIndex", () => {
  it("keeps visible, non-empty leaf blocks, defaults the end, and orders them by position", () => {
    const index = buildIndex([
      m(4, 6, 100, 40),
      m(0, 2, 0, 30),
      m(2, null, 50, 20),
      m(7, 9, 200, 0),
      m(9, 10, 220, 20, { visible: false }),
      m(10, 20, 240, 300, { hasTaggedVisibleDescendant: true }),
      m(12, 2, 260, 20),
    ]);
    expect(index.map((b) => [b.line, b.end, b.top, b.height])).toEqual([
      [0, 2, 0, 30], [2, 3, 50, 20], [4, 6, 100, 40], [12, 13, 260, 20],
    ]);
  });

  it("keeps the later, innermost element of two on one line", () => {
    const outer = m(3, 5, 10, 50);
    const inner = m(3, 4, 12, 20);
    expect(buildIndex([outer, inner]).map((b) => b.el)).toEqual([inner.el]);
  });

  it("leaves out a block placed below a later line's block, so both orders hold", () => {
    // A footnote definition from line 4 rendered at the end, after the heading of line 10.
    const index = buildIndex([m(0, 2, 0, 30), m(10, 11, 100, 20), m(4, 5, 500, 20), m(20, null, 600, 1)]);
    expect(index.map((b) => [b.line, b.top])).toEqual([[0, 0], [10, 100], [20, 600]]);
    expect(lastTopAtOrBefore(index, 550)).toBe(1);
    expect(lastLineAtOrBefore(index, 4)).toBe(0);
  });
});

describe("binary searches", () => {
  const index = buildIndex([m(0, 2, 0, 30), m(4, 6, 100, 40), m(10, 11, 300, 20)]);
  it("finds the last entry at or before a line", () => {
    expect([-1, 0, 1.5, 3.9, 4, 9.99, 10, 99].map((l) => lastLineAtOrBefore(index, l))).toEqual([-1, 0, 0, 0, 1, 1, 2, 2]);
    expect(lastLineAtOrBefore([], 3)).toBe(-1);
  });
  it("finds the last entry at or above a y", () => {
    expect([-5, 0, 99, 100, 299.5, 300, 1000].map((y) => lastTopAtOrBefore(index, y))).toEqual([-1, 0, 0, 1, 1, 2, 2]);
    expect(lastTopAtOrBefore([], 3)).toBe(-1);
  });
});

describe("measure", () => {
  function place(node: Element, top: number, height: number) {
    node.getBoundingClientRect = () =>
      ({ top, height, bottom: top + height, left: 0, right: 100, width: 100, x: 0, y: top, toJSON: () => ({}) }) as DOMRect;
  }

  it("indexes a separate raw-HTML summary inside closed details by its range, keeping hidden content out", () => {
    const root = document.createElement("main");
    root.innerHTML = '<details>' +
      '<div class="code-line" data-line="2" data-line-end="3" data-eitri-html="start:1"></div>' +
      '<summary>Visible title</summary><span data-eitri-html="end:1"></span>' +
      '<div class="code-line" data-line="4" data-line-end="5" data-eitri-html="start:2"></div>' +
      '<p>Hidden content</p><span data-eitri-html="end:2"></span>' +
      '<p class="code-line" data-line="6" data-line-end="7">Also hidden</p></details>';
    const [summaryMarker, hiddenMarker, hidden] = [...root.querySelectorAll(".code-line")];
    place(summaryMarker, 0, 0);
    place(hiddenMarker, 0, 0);
    // A regular element still uses the closed-details check even if it reports a box.
    place(hidden, 10, 20);
    const createRange = document.createRange.bind(document);
    const spy = vi.spyOn(document, "createRange").mockImplementation(() => {
      const range = createRange();
      range.getClientRects = () => (range.startOffset === 1
        ? [{ top: 40, bottom: 64, height: 24 }] : []) as unknown as DOMRectList;
      return range;
    });
    try {
      const measured = measure(root, 100);
      expect(measured.map((m) => m.visible)).toEqual([true, false, false]);
      expect(buildIndex(measured)).toEqual([{ line: 2, end: 3, top: 140, height: 24, el: summaryMarker }]);
    } finally {
      spy.mockRestore();
    }
  });
  it("reads every tagged element's lines and document position, and marks containers and hidden ones", () => {
    const root = document.createElement("main");
    root.innerHTML = [
      '<p class="code-line" data-line="0" data-line-end="2">a</p>',
      '<ul class="code-line" data-line="3" data-line-end="5"><li class="code-line" data-line="3" data-line-end="4">x</li></ul>',
      '<details class="code-line" data-line="6" data-line-end="9"><summary>t</summary>' +
        '<p class="code-line" data-line="7" data-line-end="8">hidden</p></details>',
      '<pre class="code-line" data-line="10" data-line-end="14" style="padding-top: 8px; padding-bottom: 6px"><code>c</code></pre>',
      '<div class="code-line" data-line="15"></div>',
      '<p class="code-line" data-line="x">bad</p>',
    ].join("");
    const [p, ul, li, details, hidden, pre, sentinel] = [...root.querySelectorAll(".code-line")];
    place(p, 10, 30); place(ul, 50, 40); place(li, 50, 20); place(details, 100, 24); place(hidden, 0, 0);
    place(pre, 130, 100); place(sentinel, 240, 1);
    const got = measure(root, 1000);
    expect(got.map((x) => [x.line, x.end, x.top, x.height, x.visible, x.hasTaggedVisibleDescendant])).toEqual([
      [0, 2, 1010, 30, true, false],
      [3, 5, 1050, 40, true, true],
      [3, 4, 1050, 20, true, false],
      [6, 9, 1100, 24, true, false],
      [7, 8, 1000, 0, false, false],
      [10, 14, 1138, 86, true, false],
      [15, null, 1240, 1, true, false],
    ]);
    expect(buildIndex(got).map((b) => b.line)).toEqual([0, 3, 6, 10, 15]);
    details.setAttribute("open", "");
    place(hidden, 130, 20);
    const opened = measure(root, 0);
    expect(opened.find((x) => x.line === 7)!.visible).toBe(true);
    expect(opened.find((x) => x.line === 6)!.hasTaggedVisibleDescendant).toBe(true);
  });
});
