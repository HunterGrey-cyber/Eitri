import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { buildIndex, measure } from "./lineindex";
import { foldRows, rowsIn } from "./rows";

describe("folding line fragments into rows", () => {
  it("joins fragments that share a line and gives each row its leading, up to half the gap to the next", () => {
    const rows = foldRows([
      { top: 0, bottom: 16 }, { top: 2, bottom: 14 }, // two fragments of one line (a word in another font)
      { top: 22, bottom: 38 },
      { top: 20, bottom: 44 }, // an inline formula taller than the line it rides on
      { top: 100, bottom: 116 }, // the next paragraph, after a block gap
    ], 22);
    // The formula's row is taller than a line: it gets no leading.
    expect(rows).toEqual([
      { top: -3, bottom: 18 }, { top: 20, bottom: 44 }, { top: 97, bottom: 119 },
    ]);
  });

  it("keeps lines apart whose fragments only touch", () => {
    expect(foldRows([{ top: 0, bottom: 20 }, { top: 18, bottom: 38 }], 20).length).toBe(2);
  });
});

/* Text rects come from `data-r` on the text's parent ("top,bottom;top,bottom" in document coordinates), element
   boxes from `data-b` ("top,bottom"): a layout written out by hand. */
describe("the rows of the rendered document", () => {
  let root: HTMLElement;
  const proto = Range.prototype as Range & { getClientRects?: () => DOMRect[] };
  const rect = (top: number, bottom: number) =>
    ({ top: top - window.scrollY, bottom: bottom - window.scrollY, height: bottom - top, left: 0, right: 100, width: 100, x: 0, y: top, toJSON: () => ({}) }) as DOMRect;
  const pairs = (v: string | null | undefined) =>
    (v ?? "").split(";").filter(Boolean).map((p) => p.split(",").map(Number) as [number, number]);
  const original = Element.prototype.getBoundingClientRect;
  beforeEach(() => {
    Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
    Object.defineProperty(proto, "getClientRects", {
      configurable: true,
      value: function (this: Range) {
        const el = this.startContainer.parentElement;
        return pairs(el?.getAttribute("data-r")).map(([t, b]) => rect(t, b));
      },
    });
    Element.prototype.getBoundingClientRect = function (this: Element) {
      const [box] = pairs(this.getAttribute("data-b"));
      return box ? rect(box[0], box[1]) : rect(0, 0);
    };
    document.body.innerHTML = `<main id="r">
      <p class="code-line" data-line="0" data-line-end="1" data-b="0,40"><span data-r="0,16;20,36">a wrapped paragraph</span></p>
      <ul><li class="code-line" data-line="2" data-line-end="5" data-b="60,140"><span data-r="60,76">an outer item</span>
        <ul><li class="code-line" data-line="3" data-line-end="4" data-b="80,100"><span data-r="82,98">an inner item</span></li></ul></li></ul>
      <p class="code-line" data-line="6" data-line-end="7" data-b="160,200"><span data-r="160,176">text <span class="katex" data-b="158,200"><span data-r="170,180">x</span><span data-r="190,199">y</span></span></span></p>
      <div class="mdp-diagram code-line" data-line="8" data-line-end="12" data-b="220,520"><svg data-b="220,520"></svg><span data-r="300,310">a label</span></div>
      <p class="code-line" data-line="13" data-line-end="14" data-b="540,560" hidden><span data-r="540,556">hidden</span></p>
      <p class="code-line" data-line="15" data-line-end="16" data-b="580,600"><span data-r="580,596">last</span></p>
    </main>`;
    root = document.getElementById("r")!;
  });
  afterEach(() => {
    delete (proto as { getClientRects?: unknown }).getClientRects;
    Element.prototype.getBoundingClientRect = original;
  });

  it("are a paragraph's lines, a list item's own text above its nested list, a formula as part of its line and a diagram as one row", () => {
    const index = buildIndex(measure(root, 0));
    const rows = rowsIn(root, index, 0, 700, 20, 0);
    expect(rows.map((r) => [r.top, r.bottom])).toEqual([
      [-2, 18], [18, 38], // the paragraph's two lines
      [58, 78], [80, 100], // the outer item's own line, the inner item's line
      [158, 200], // "text" and the formula riding on it
      [220, 520], // the diagram: its label is not read as text
      [578, 598], // the last paragraph; the hidden one has no row
    ]);
  });

  it("take in a list item's own line when the document opens with it, above the nested list's first block", () => {
    document.body.innerHTML = `<main id="r"><ul><li class="code-line" data-line="0" data-line-end="2" data-b="0,40"><span data-r="2,18">outer</span>
      <ul><li class="code-line" data-line="1" data-line-end="2" data-b="20,40"><span data-r="22,38">inner</span></li></ul></li></ul>
      <p class="code-line" data-line="3" data-line-end="4" data-b="50,70"><span data-r="52,68">para</span></p></main>`;
    root = document.getElementById("r")!;
    const index = buildIndex(measure(root, 0));
    expect(index.map((b) => b.line)).toEqual([1, 3]);
    expect(rowsIn(root, index, 0, 100, 20, 0).map((r) => [r.top, r.bottom])).toEqual([[0, 20], [20, 40], [50, 70]]);
  });

  it("are worked out around a span, not over the whole document", () => {
    const index = buildIndex(measure(root, 0));
    const rows = rowsIn(root, index, 150, 210, 20, 0);
    expect(rows.map((r) => r.top)).toEqual([158]);
  });
});
