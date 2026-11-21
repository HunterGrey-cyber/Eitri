import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { BlockCursor } from "./block";
import { textFrom } from "./text-from";

/* Layouts jsdom cannot give: a table whose cells differ in height, cells of several lines side by side, text that
   runs right to left, a cell spanning rows. Each test says where every character of a text node is; a range over one
   character reports that box, a range over a node or part of one the rows its characters are on, and a node nobody
   placed has no box. An element's own box holds the characters inside it, unless the test gives it one (a table cell
   is as tall as its row). */
interface Spot { left: number; top: number; width: number }
const spots = new Map<Text, Spot[]>();
const boxes = new Map<Element, DOMRect>();
const HEIGHT = 20;
const rect = (left: number, top: number, width: number, height = HEIGHT): DOMRect =>
  ({ left, top, width, height, right: left + width, bottom: top + height, x: left, y: top, toJSON: () => ({}) }) as DOMRect;

/** Places the characters of `node` (one code unit each) at `at`. */
function put(node: Node, at: Spot[]): Text {
  const t = node as Text;
  spots.set(t, at);
  return t;
}
/** `n` characters of `width` from x `left` on the row at `top`. */
const run = (n: number, left: number, top: number, width = 8): Spot[] =>
  Array.from({ length: n }, (_, i) => ({ left: left + i * width, top, width }));

const proto = Range.prototype as Range & { getClientRects?: () => DOMRect[] };
let had: PropertyDescriptor | undefined;
let hadBox: PropertyDescriptor | undefined;
const realElementBox = Element.prototype.getBoundingClientRect;

function elementBox(el: Element): DOMRect {
  const own = boxes.get(el);
  if (own) return own;
  const all: DOMRect[] = [];
  const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    for (const s of spots.get(n as Text) ?? []) all.push(rect(s.left, s.top, s.width));
  }
  if (all.length === 0) return rect(0, 0, 0, 0);
  const left = Math.min(...all.map((r) => r.left));
  const top = Math.min(...all.map((r) => r.top));
  return rect(left, top, Math.max(...all.map((r) => r.right)) - left, Math.max(...all.map((r) => r.bottom)) - top);
}

function rectsOf(r: Range): DOMRect[] {
  const node = r.startContainer;
  const at = node instanceof Text ? spots.get(node) : undefined;
  if (!(node instanceof Text) || !at) return [];
  if (node === r.endContainer && r.endOffset - r.startOffset === 1) {
    const s = at[r.startOffset];
    return s ? [rect(s.left, s.top, s.width)] : [];
  }
  const rows = new Map<number, [number, number]>();
  const end = node === r.endContainer ? r.endOffset : node.data.length;
  for (const s of at.slice(r.startOffset, end)) {
    const known = rows.get(s.top);
    rows.set(s.top, known ? [Math.min(known[0], s.left), Math.max(known[1], s.left + s.width)] : [s.left, s.left + s.width]);
  }
  return [...rows].map(([top, [l, right]]) => rect(l, top, right - l));
}

beforeEach(() => {
  spots.clear();
  boxes.clear();
  Element.prototype.getBoundingClientRect = function (this: Element) { return elementBox(this); };
  had = Object.getOwnPropertyDescriptor(proto, "getClientRects");
  hadBox = Object.getOwnPropertyDescriptor(proto, "getBoundingClientRect");
  Object.defineProperty(proto, "getClientRects", { configurable: true, value: function (this: Range) { return rectsOf(this); } });
  Object.defineProperty(proto, "getBoundingClientRect", {
    configurable: true, value: function (this: Range) { return rectsOf(this)[0] ?? rect(0, 0, 0, 0); },
  });
  document.body.innerHTML = "";
});
afterEach(() => {
  Element.prototype.getBoundingClientRect = realElementBox;
  for (const [name, d] of [["getClientRects", had], ["getBoundingClientRect", hadBox]] as const) {
    if (d) Object.defineProperty(proto, name, d);
    else delete (proto as unknown as Record<string, unknown>)[name];
  }
  document.body.innerHTML = "";
});

const mount = (html: string): HTMLElement => {
  const root = document.createElement("div");
  root.innerHTML = html;
  document.body.append(root);
  return root;
};
const at = (t: Text, offset: number): Range => { const r = document.createRange(); r.setStart(t, offset); return r; };

describe("the first character at the band, in text whose order is not the page's", () => {
  it("is found in a table row whose cells differ in height", () => {
    const root = mount("<table><tbody><tr><td>AAAAAA</td><td>B</td></tr><tr><td>C</td></tr></tbody></table>");
    const [a, b, c] = [...root.querySelectorAll("td")].map((td) => td.firstChild);
    // A is three lines tall (the last at 40), B is a short cell centred beside it, C is the next row.
    put(a!, [...run(2, 0, 0), ...run(2, 0, 20), ...run(2, 0, 40)]);
    put(b!, run(1, 100, 20));
    put(c!, run(1, 0, 70));
    // The band is A's last line: A reaches below its top, B does not, C does. Halving over the three would skip A.
    const found = textFrom(root.querySelector("table")!, { top: 40, bottom: 60 })!;
    expect(found.startContainer).toBe(a);
    expect(found.startOffset).toBe(4);
  });
});

describe("a short cell centred beside a tall one", () => {
  /* A cell as tall as its row, as a table lays it out, whatever its text's height. */
  const cell = (td: Element, top: number, left: number) => boxes.set(td, rect(left, top, 90, 60));
  function table(shortFirst: boolean) {
    const html = shortFirst ? "<td>B</td><td>AAAAAA</td>" : "<td>AAAAAA</td><td>B</td>";
    const root = mount(`<table><tbody><tr>${html}</tr><tr><td>C</td></tr></tbody></table>`);
    const tds = [...root.querySelectorAll("td")];
    const [tdA, tdB] = shortFirst ? [tds[1], tds[0]] : [tds[0], tds[1]];
    const a = tdA.firstChild as Text;
    const b = tdB.firstChild as Text;
    // A is three lines (0, 20, 40); B, one line, is centred in its cell, on A's second line; C is the next row.
    const ax = shortFirst ? 100 : 0;
    const bx = shortFirst ? 0 : 100;
    put(a, [...run(2, ax, 0), ...run(2, ax, 20), ...run(2, ax, 40)]);
    put(b, run(1, bx, 20));
    put(tds[2].firstChild!, run(1, 0, 70));
    cell(tdA, 0, ax);
    cell(tdB, 0, bx);
    boxes.set(root.querySelector("tr")!, rect(0, 0, 190, 60));
    return { root, a, b };
  }

  for (const shortFirst of [true, false]) {
    it(`leaves the tall cell's first line its own, the short cell ${shortFirst ? "before" : "after"} it`, () => {
      const { root, a } = table(shortFirst);
      const band = { top: 0, bottom: 20 };
      const start = () => textFrom(root.querySelector("tr")!, band);
      expect([start()!.startContainer, start()!.startOffset]).toEqual([a, 0]);
      const block = new BlockCursor(root);
      block.draw(band, start);
      expect(block.el.hidden).toBe(false);
      expect([block.caretRange()!.startContainer, block.caretRange()!.startOffset]).toEqual([a, 0]);
      expect(block.move("$")).toBe(true);
      expect([block.caretRange()!.startContainer, block.caretRange()!.startOffset]).toEqual([a, 1]);
      block.remove();
    });
  }

  it("puts the short cell's line, the tall cell's second, on one row", () => {
    const { root, a, b } = table(true);
    const band = { top: 20, bottom: 40 };
    const block = new BlockCursor(root);
    block.draw(band, () => textFrom(root.querySelector("tr")!, band));
    expect(block.caretRange()!.startContainer).toBe(b);
    expect(block.move("$")).toBe(true);
    expect([block.caretRange()!.startContainer, block.caretRange()!.startOffset]).toEqual([a, 3]);
    block.draw(band, () => textFrom(root.querySelector("tr")!, band));
    expect(block.index()).toBe(2);
    block.remove();
  });
});

describe("a row of a table with cells of several lines", () => {
  function table() {
    const root = mount("<table><tbody><tr><td>One<br>Two</td><td>Left<br>Right</td></tr></tbody></table>");
    const [one, two, left, right] = [...root.querySelectorAll("td")].flatMap((td) => [...td.childNodes].filter((n) => n.nodeType === 3));
    put(one, run(3, 0, 0)); put(two, run(3, 0, 20)); put(left, run(4, 100, 0)); put(right, run(5, 100, 20));
    return { root, one: one as Text, two: two as Text, left: left as Text, right: right as Text };
  }

  it("is read whole, so a click lands in the cell beside, and `$` goes to its last character", () => {
    const { root, one, left } = table();
    const block = new BlockCursor(root);
    // A click on the `e` of `Left` (x 116 to 124), on the first row: not past the end of `One`.
    expect(block.placeNear({ top: 0, bottom: 20 }, 118, at(one, 0))).toBe(true);
    block.draw({ top: 0, bottom: 20 }, () => at(one, 0));
    expect(block.caretRange()!.startContainer).toBe(left);
    expect(block.caretRange()!.startOffset).toBe(2);
    expect(block.move("$")).toBe(true);
    expect(block.caretRange()!.startContainer).toBe(left);
    expect(block.caretRange()!.startOffset).toBe(3);
    expect(block.move("0")).toBe(true);
    expect(block.caretRange()!.startContainer).toBe(one);
    expect(block.caretRange()!.startOffset).toBe(0);
    block.remove();
  });

  it("counts the second row from its own first character", () => {
    const { root, two, right } = table();
    const block = new BlockCursor(root);
    block.draw({ top: 20, bottom: 40 }, () => at(two, 0));
    expect(block.index()).toBe(0);
    expect(block.move("$")).toBe(true);
    block.draw({ top: 20, bottom: 40 }, () => at(two, 0));
    expect(block.caretRange()!.startContainer).toBe(right);
    expect(block.index()).toBe(3 + 4);
    block.remove();
  });
});

describe("a row that runs right to left", () => {
  it("is searched for the character at x, not for the one whose right edge is past it", () => {
    const root = mount("<p>\u05d0\u05d1\u05d2\u05d3</p>");
    const t = root.querySelector("p")!.firstChild as Text;
    // The first character is the rightmost: x 24 to 32, then 16, 8 and 0 for the last.
    put(t, [24, 16, 8, 0].map((left) => ({ left, top: 0, width: 8 })));
    const block = new BlockCursor(root);
    expect(block.placeNear({ top: 0, bottom: 20 }, 28, at(t, 0))).toBe(true);
    block.draw({ top: 0, bottom: 20 }, () => at(t, 0));
    expect(block.caretRange()!.startOffset).toBe(0);
    // A click left of the row's end is the character at the left.
    expect(block.placeNear({ top: 0, bottom: 20 }, 3, at(t, 0))).toBe(true);
    block.draw({ top: 0, bottom: 20 }, () => at(t, 0));
    expect(block.caretRange()!.startOffset).toBe(3);
    block.remove();
  });
});

describe("a cell spanning rows", () => {
  /* Raw HTML's `rowspan`: the spanning cell's text is centred in it, on the middle row of the three it spans, beside
     that row's own cell; each row's box is its own height, so the spanning cell is drawn below its row's box. */
  function table(before = 0) {
    const plain = (i: number) => `<tr><td>r${i}</td></tr>`;
    const rows = Array.from({ length: before }, (_, i) => plain(i)).join("");
    const after = Array.from({ length: 6 }, (_, i) => plain(before + 3 + i)).join("");
    const root = mount(`<table><tbody>${rows}<tr><td rowspan="3">SPAN</td><td>A</td></tr><tr><td>B</td></tr>` +
      `<tr><td>C</td></tr>${after}</tbody></table>`);
    const trs = [...root.querySelectorAll("tr")];
    trs.forEach((tr, i) => {
      boxes.set(tr, rect(0, 20 * i, 190, 20));
      const cells = [...tr.querySelectorAll("td")];
      const own = cells[cells.length - 1];
      put(own.firstChild!, run(own.textContent!.length, 100, 20 * i));
    });
    const spanning = root.querySelector("td[rowspan]")!;
    boxes.set(spanning, rect(0, 20 * before, 90, 60));
    const span = put(spanning.firstChild!, run(4, 0, 20 * (before + 1)));
    const b = trs[before + 1].querySelector("td")!.firstChild as Text;
    return { root, span, b, band: { top: 20 * (before + 1), bottom: 20 * (before + 2) } };
  }
  const where = (block: BlockCursor) => [block.caretRange()!.startContainer, block.caretRange()!.startOffset];

  for (const before of [0, 9]) {
    it(`is on the rows it spans beside their own cells${before ? ", among many rows" : ""}`, () => {
      const { root, span, b, band } = table(before);
      // The row's first character is the spanning cell's, though its row's box ends above the band.
      const start = () => textFrom(root, band);
      expect([start()!.startContainer, start()!.startOffset]).toEqual([span, 0]);
      const block = new BlockCursor(root);
      // A click on the spanning cell's text puts the block there; the row runs on into the next cell.
      expect(block.placeNear(band, 12, start())).toBe(true);
      block.draw(band, start);
      expect(where(block)).toEqual([span, 1]);
      expect(block.move("$")).toBe(true);
      expect(where(block)).toEqual([b, 0]);
      block.draw(band, start);
      expect(block.index()).toBe(4);
      // From the row's own cell, the row still holds the spanning cell's text.
      expect(block.placeNear(band, 4, at(b, 0))).toBe(true);
      block.draw(band, start);
      expect(where(block)).toEqual([span, 0]);
      expect(block.move("0")).toBe(false);
      block.remove();
    });
  }
});

describe("a cell spanning rows, its neighbours", () => {
  it("gives the first text below a row of images from the nearest row, not from the leftmost cell", () => {
    // Images only in the first row, the left one spanning two rows; text in the second row's right cell and in the
    // third row's left cell, which is further left but a row further down.
    const root = mount(`<table><tbody><tr><td rowspan="2"><img></td><td><img></td></tr><tr><td>near</td></tr>` +
      `<tr><td>far</td><td></td></tr></tbody></table>`);
    for (const el of [root.querySelector("table")!, root.querySelector("tbody")!]) boxes.set(el, rect(0, 0, 190, 60));
    const trs = [...root.querySelectorAll("tr")];
    trs.forEach((tr, i) => boxes.set(tr, rect(0, 20 * i, 190, 20)));
    const [spanning, right] = trs[0].querySelectorAll("td");
    boxes.set(spanning, rect(0, 0, 90, 40));
    boxes.set(right, rect(100, 0, 90, 20));
    boxes.set(trs[2].querySelectorAll("td")[1], rect(100, 40, 90, 20));
    const near = put(trs[1].querySelector("td")!.firstChild!, run(4, 100, 20));
    put(trs[2].querySelector("td")!.firstChild!, run(3, 0, 40));
    const start = textFrom(root, { top: 0, bottom: 20 })!;
    expect([start.startContainer, start.startOffset]).toEqual([near, 0]);
  });

  it("reads no rows above the band for a rowspan=0 cell in the section's last row", () => {
    const n = 200;
    const plain = Array.from({ length: n - 1 }, (_, i) => `<tr><td>r${i}</td></tr>`).join("");
    const root = mount(`<table><tbody>${plain}<tr><td rowspan="0">END</td></tr></tbody></table>`);
    expect(root.querySelector<HTMLTableCellElement>("td[rowspan]")!.rowSpan).toBe(0);
    const trs = [...root.querySelectorAll("tr")];
    trs.forEach((tr, i) => {
      boxes.set(tr, rect(0, 20 * i, 190, 20));
      const td = tr.querySelector("td")!;
      put(td.firstChild!, run(td.textContent!.length, 0, 20 * i));
    });
    let cellReads = 0;
    const box = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function (this: Element) {
      if (this.tagName === "TD") cellReads++;
      return box.call(this);
    };
    try {
      const start = textFrom(root, { top: 20 * 150, bottom: 20 * 151 })!;
      expect(start.startContainer.textContent).toBe("r150");
    } finally {
      Element.prototype.getBoundingClientRect = box;
    }
    // From row 0 it is a read of every cell above the band: 150 here.
    expect(cellReads).toBeLessThanOrEqual(4);
  });
});

describe("a disclosure closing in a table cell", () => {
  it("drops the row it read: $ goes to the last character still drawn", () => {
    const root = mount("<table><tbody><tr><td>abc</td><td><details open><summary>S</summary>XYZ</details></td></tr>" +
      "</tbody></table>");
    // The left cell's text is on its second line; the right cell's body is beside it, under its summary. The row and
    // both cells keep their sizes when the body folds.
    const [left, right] = root.querySelectorAll("td");
    boxes.set(root.querySelector("tr")!, rect(0, 0, 190, 60));
    boxes.set(left, rect(0, 0, 90, 60));
    boxes.set(right, rect(100, 0, 90, 60));
    const abc = put(left.firstChild!, run(3, 0, 20));
    put(root.querySelector("summary")!.firstChild!, run(1, 100, 0));
    const details = root.querySelector("details")!;
    const xyz = put(details.lastChild!, run(3, 100, 20));
    const band = { top: 20, bottom: 40 };
    const block = new BlockCursor(root);
    block.draw(band, () => at(abc, 0));
    const where = () => [block.caretRange()!.startContainer, block.caretRange()!.startOffset];
    expect(block.move("$")).toBe(true);
    block.draw(band, () => at(abc, 0));
    expect(where()).toEqual([xyz, 2]);
    expect(block.move("0")).toBe(true);
    block.draw(band, () => at(abc, 0));
    expect(where()).toEqual([abc, 0]);
    // A click on the summary: the body folds, and nothing but the attribute changes.
    details.open = false;
    spots.delete(xyz);
    expect(block.move("$")).toBe(true);
    block.draw(band, () => at(abc, 0));
    expect(where()).toEqual([abc, 2]);
    block.remove();
  });
});

describe("visual mode's cursor at a line's end", () => {
  it("keeps a line's own last character, and leaves no block on a line that has none", () => {
    const root = mount("<p>one<br><img><br>two</p><p>end</p><p><img></p>");
    const [p, last, imageOnly] = root.querySelectorAll("p");
    const [one, two] = [p.firstChild as Text, p.lastChild as Text];
    put(one, run(3, 0, 0));
    put(two, run(3, 0, 40));
    boxes.set(p.querySelector("img")!, rect(0, 20, 40, 20));
    const end = put(last.firstChild!, run(3, 0, 70));
    boxes.set(imageOnly, rect(0, 100, 40, 20));
    boxes.set(imageOnly.querySelector("img")!, rect(0, 100, 40, 20));
    const block = new BlockCursor(root);
    const shownAt = (band: { top: number; bottom: number }) => {
      block.draw(band, () => textFrom(root, band));
      return block.el.hidden ? null : [block.caretRange()!.startContainer, block.caretRange()!.startOffset];
    };
    // Before the first break: the end of `one`'s line.
    expect(block.placeAtCursor({ node: p, offset: 1 })).toBe(true);
    expect(shownAt({ top: 0, bottom: 20 })).toEqual([one, 2]);
    // After the image, before the second break: the image's line has no character, so no block, there or above.
    expect(block.placeAtCursor({ node: p, offset: 3 })).toBe(true);
    expect(block.caretBox()).toBeNull();
    expect(shownAt({ top: 20, bottom: 40 })).toBeNull();
    // The end of a paragraph after its text: its last character.
    expect(block.placeAtCursor({ node: last, offset: 1 })).toBe(true);
    expect(shownAt({ top: 70, bottom: 90 })).toEqual([end, 2]);
    // After an image alone in the next paragraph: drawn below `end`'s line, so no block either.
    expect(block.placeAtCursor({ node: imageOnly, offset: 1 })).toBe(true);
    expect(block.caretBox()).toBeNull();
    expect(shownAt({ top: 100, bottom: 120 })).toBeNull();
    // A place that is not on the page changes nothing.
    expect(block.placeAtCursor({ node: document.createTextNode("x"), offset: 0 })).toBe(false);
    block.remove();
  });
});

describe("h and l in raw HTML", () => {
  it("stop at the end of a paragraph that no source-mapped block holds", () => {
    // Allowed raw HTML is marked by boundaries beside it, not around it: both paragraphs sit under the root.
    const root = mount("<p>one</p><p>two</p>");
    const [one, two] = [...root.querySelectorAll("p")].map((e) => e.firstChild as Text);
    put(one, run(3, 0, 0));
    put(two, run(3, 0, 30));
    const block = new BlockCursor(root);
    block.draw({ top: 0, bottom: 20 }, () => at(one, 0));
    expect(block.move("l")).toBe(true);
    expect(block.move("l")).toBe(true);
    expect(block.caretRange()!.startOffset).toBe(2);
    expect(block.move("l")).toBe(false);
    expect(block.caretRange()!.startContainer).toBe(one);
    block.remove();
  });
});
