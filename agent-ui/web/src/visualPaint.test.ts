// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { lineBand, paintSelection, selectionBoxes } from "./visualPaint";

/* jsdom lays nothing out. Each text named here lies on one line at `top`, `left`, `width` px (`rows` lines of it
   20 px apart, the width split evenly), its characters evenly wide; an element's box is `boxes` by tag. */
type Fake = { top: number; height: number; left: number; width: number; rows?: number };
let texts: Map<string, Fake>;
let elements: Map<Element, { top: number; height: number; left: number; width: number }>;
const restore: (() => void)[] = [];

beforeEach(() => {
  texts = new Map();
  elements = new Map();
  const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
  const had = Object.getOwnPropertyDescriptor(proto, "getClientRects");
  Object.defineProperty(proto, "getClientRects", {
    configurable: true,
    value: function (this: Range) {
      const node = this.startContainer;
      const f = node.nodeType === Node.TEXT_NODE ? texts.get((node as Text).data) : undefined;
      if (!f) return [];
      const n = (node as Text).data.length;
      const rows = f.rows ?? 1;
      const per = Math.ceil(n / rows);
      const end = this.endContainer === node ? this.endOffset : n;
      const out: DOMRect[] = [];
      for (let r = 0; r < rows; r++) {
        const a = Math.max(this.startOffset, r * per);
        const b = Math.min(end, (r + 1) * per);
        if (b <= a) continue;
        const w = f.width / per;
        const top = f.top + 20 * r;
        const left = f.left + (a - r * per) * w;
        out.push({ top, height: f.height, bottom: top + f.height, left, right: left + (b - a) * w, width: (b - a) * w,
          x: left, y: top, toJSON: () => ({}) } as DOMRect);
      }
      return out;
    },
  });
  const box = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const b = elements.get(this) ?? { top: 0, height: 0, left: 0, width: 1000 };
    return { top: b.top, height: b.height, bottom: b.top + b.height, left: b.left, right: b.left + b.width,
      width: b.width, x: b.left, y: b.top, toJSON: () => ({}) } as DOMRect;
  };
  restore.push(() => {
    if (had) Object.defineProperty(proto, "getClientRects", had);
    else delete (proto as unknown as Record<string, unknown>).getClientRects;
    Element.prototype.getBoundingClientRect = box;
  });
});
afterEach(() => {
  for (const fn of restore.splice(0)) fn();
  document.body.replaceChildren();
  document.getSelection()?.removeAllRanges();
});

const ORIGIN = { x: 0, y: 0 };
function table(): HTMLElement {
  document.body.innerHTML = '<div class="message-list" style="line-height: 20px"><div class="table-scroll"><table>' +
    "<tbody>\n<tr><td>shell</td><td>48,315</td><td>57,077</td></tr>\n<tr><td>panel</td><td>22,486</td><td>20,710</td>" +
    "</tr>\n</tbody></table></div></div>";
  texts.set("shell", { top: 102, height: 18, left: 10, width: 50 });
  texts.set("48,315", { top: 100, height: 22, left: 90, width: 60 });
  texts.set("57,077", { top: 100, height: 22, left: 180, width: 60 });
  texts.set("panel", { top: 142, height: 18, left: 10, width: 50 });
  const rows = document.querySelectorAll("tr");
  elements.set(rows[0], { top: 90, height: 40, left: 0, width: 300 });
  elements.set(rows[1], { top: 130, height: 40, left: 0, width: 300 });
  return document.querySelector(".message-list")!;
}
function text(data: string): Text {
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) if ((n as Text).data === data) return n as Text;
  throw new Error(data);
}
function range(from: string, a: number, to: string, b: number): Range {
  const r = document.createRange();
  r.setStart(text(from), a);
  r.setEnd(text(to), b);
  return r;
}

describe("VISUAL's boxes", () => {
  it("draw each table cell's selected text apart, a row's boxes at one height, its line's", () => {
    table();
    expect(selectionBoxes(range("shell", 0, "57,077", 1), 7, ORIGIN)).toEqual([
      { top: 100, bottom: 122, left: 10, right: 60 },
      { top: 100, bottom: 122, left: 90, right: 150 },
      { top: 100, bottom: 122, left: 180, right: 190 },
    ]);
  });

  it("join a row's text over a space between two spans, and give a wrapped paragraph's rows the leading so they meet", () => {
    document.body.innerHTML = '<div class="message-list"><p style="line-height: 24px"><b>fn</b> <i>main</i></p>' +
      '<p style="line-height: 24px">a paragraph long enough to wrap</p></div>';
    texts.set("fn", { top: 0, height: 18, left: 0, width: 16 });
    texts.set(" ", { top: 0, height: 18, left: 16, width: 8 });
    texts.set("main", { top: 0, height: 18, left: 24, width: 32 });
    texts.set("a paragraph long enough to wrap", { top: 40, height: 18, left: 0, width: 300, rows: 2 });
    const boxes = selectionBoxes(range("fn", 0, "a paragraph long enough to wrap", 31), 7, ORIGIN);
    expect(boxes[0]).toEqual({ top: -3, bottom: 21, left: 0, right: 56 });
    expect(boxes[1].bottom).toBe(boxes[2].top);
  });

  it("are cut to the sides of a block that scrolls sideways", () => {
    table();
    elements.set(document.querySelector(".table-scroll")!, { top: 0, height: 400, left: 0, width: 160 });
    // 57,077 lies wholly past the block's right side, at 180-240: nothing of it is drawn.
    expect(selectionBoxes(range("48,315", 0, "57,077", 6), 7, ORIGIN))
      .toEqual([{ top: 100, bottom: 122, left: 90, right: 150 }]);
  });
});

describe("what is not selected text", () => {
  it("leaves out VISUAL's chrome inside a row, as y does: buttons, fold markers, a card's actions", () => {
    document.body.innerHTML = '<div class="message-list"><p style="line-height: 18px">before <button>Approve</button> <span class="fold-marker">' +
      '\u25b8</span> <span data-nav-action="x">open</span> after</p></div>';
    texts.set("before ", { top: 0, height: 18, left: 0, width: 56 });
    texts.set("Approve", { top: 0, height: 18, left: 56, width: 60 });
    texts.set("\u25b8", { top: 0, height: 18, left: 120, width: 10 });
    texts.set("open", { top: 0, height: 18, left: 134, width: 30 });
    texts.set(" after", { top: 0, height: 18, left: 300, width: 48 });
    const r = range("before ", 0, " after", 6);
    expect(selectionBoxes(r, 7, ORIGIN)).toEqual([
      { top: 0, bottom: 18, left: 0, right: 56 },
      { top: 0, bottom: 18, left: 300, right: 348 },
    ]);
  });
});

describe("V-LINE's band", () => {
  it("covers the table rows the selection starts and ends in, whole, across the list", () => {
    table();
    const r = range("shell", 0, "panel", 5);
    expect(lineBand(r, selectionBoxes(r, 7, ORIGIN), 500, ORIGIN)).toEqual({ top: 90, bottom: 170, left: 0, right: 500 });
  });
});

describe("paintSelection", () => {
  it("draws boxes for VISUAL, one band for V-LINE, nothing for CARET or BROWSE", () => {
    const list = table();
    const layer = document.createElement("div");
    list.append(layer);
    document.getSelection()!.addRange(range("shell", 0, "48,315", 6));
    paintSelection(list, layer, "visual");
    expect(layer.children.length).toBe(2);
    paintSelection(list, layer, "vline");
    expect(layer.children.length).toBe(1);
    expect((layer.children[0] as HTMLElement).style.top).toBe("90px");
    paintSelection(list, layer, "caret");
    expect(layer.children.length).toBe(0);
    paintSelection(list, layer, "vline");
    paintSelection(list, layer, null);
    expect(layer.children.length).toBe(0);
  });
});
