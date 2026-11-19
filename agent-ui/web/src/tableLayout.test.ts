// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderMarkdown } from "./markdown";
import { buildTableList, chooseLayout, countLines, forgetTables, layoutTables, MAX_CELL_LINES, RESIZE_SLACK } from "./tableLayout";

const line = (top: number, height = 18, width = 40) => ({ top, bottom: top + height, height, width });

describe("countLines", () => {
  it("counts the lines the pieces of one cell fall on, however its boxes differ in height", () => {
    // Text 18 tall and a code span 22 tall on the same line, then a second and a third line 26 apart.
    expect(countLines([line(0), line(-2, 22), line(26), line(52, 22)])).toBe(3);
  });

  it("is zero for an empty cell and ignores boxes with no width, a break opportunity at a line's end among them", () => {
    expect(countLines([])).toBe(0);
    expect(countLines([line(0), line(26, 18, 0)])).toBe(1);
  });

  it("does not depend on the order the boxes come in", () => {
    expect(countLines([line(52), line(0), line(26)])).toBe(3);
  });
});

describe("chooseLayout", () => {
  const fits = { boxWidth: 520, tableWidth: 520, cellLines: [1, 2, 4, 3] };

  it("keeps the grid while the table fits its box and no cell passes four lines", () => {
    expect(MAX_CELL_LINES).toBe(4);
    expect(chooseLayout(fits)).toBe("grid");
  });

  it("goes to the list when one cell needs a fifth line", () => {
    expect(chooseLayout({ ...fits, cellLines: [1, 5] })).toBe("list");
  });

  it("goes to the list when the table is wider than its box, and tolerates a pixel of rounding", () => {
    expect(chooseLayout({ ...fits, tableWidth: 521 })).toBe("grid");
    expect(chooseLayout({ ...fits, tableWidth: 522 })).toBe("list");
  });
});

describe("buildTableList", () => {
  function tableOf(markdown: string): HTMLTableElement {
    const box = document.createElement("div");
    box.innerHTML = renderMarkdown(markdown);
    return box.querySelector("table")!;
  }

  it("makes one list per row with the header as each cell's label, keeping the cell's own markup", () => {
    const list = buildTableList(tableOf("| 想了解 | 从这里读 |\n|---|---|\n| **Agent** 对话 | `a/b` [x](https://example.com/) |\n| 窗口 | 无 |"));
    expect(list.className).toBe("table-list");
    const rows = Array.from(list.children) as HTMLElement[];
    expect(rows.map((r) => r.tagName + "." + r.className)).toEqual(["UL.table-list-row", "UL.table-list-row"]);
    const first = Array.from(rows[0].children);
    expect(first.map((c) => c.tagName + "." + c.className)).toEqual(["LI.table-list-cell", "LI.table-list-cell"]);
    expect(first[0].querySelector(".table-list-label")!.textContent).toBe("想了解:");
    expect(first[0].textContent).toBe("想了解: Agent 对话");
    expect(first[0].querySelector("strong:not(.table-list-label)")!.textContent).toBe("Agent");
    expect(first[1].querySelector("code")!.textContent).toBe("a/b");
    expect(first[1].querySelector("a")!.getAttribute("href")).toBe("https://example.com/");
  });

  it("keeps a link in a header in its label", () => {
    const list = buildTableList(tableOf("| [docs](https://example.com/) |\n|---|\n| one |"));
    expect(list.querySelector(".table-list-label a")?.getAttribute("href")).toBe("https://example.com/");
    expect(list.querySelector(".table-list-label")!.textContent).toBe("docs:");
  });

  it("leaves the table's cells where they are, drops an empty cell, and writes no label for a header with no text", () => {
    const table = tableOf("| a | |\n|---|---|\n| one | two |\n| | three |");
    const before = table.outerHTML;
    const list = buildTableList(table);
    expect(table.outerHTML).toBe(before);
    const rows = Array.from(list.children);
    expect(rows[0].textContent).toBe("a: one" + "two");
    expect(rows[1].textContent).toBe("three");
    expect(rows[1].querySelector(".table-list-label")).toBeNull();
  });

  it("builds from the table's own rows, not the rows of a table inside a cell", () => {
    const table = tableOf("| a |\n|---|\n| <table><tr><td>inner</td></tr></table> |\n| two |");
    expect(buildTableList(table).children).toHaveLength(2);
  });
});

/* jsdom has no layout: each test says what the engine would have measured. */
describe("layoutTables", () => {
  let observers: { callback: ResizeObserverCallback; seen: Set<Element> }[];
  let box: HTMLElement;

  beforeEach(() => {
    observers = [];
    vi.stubGlobal("ResizeObserver", class {
      readonly seen = new Set<Element>();
      constructor(readonly callback: ResizeObserverCallback) { observers.push(this); }
      observe(el: Element) { this.seen.add(el); }
      unobserve(el: Element) { this.seen.delete(el); }
      disconnect() { this.seen.clear(); }
    });
    // A cell's `data-lines` is the number of lines its text takes at the current width, all on its first text node. An
    // element's range (what `selectNodeContents(cell)` would ask for) answers with the one tall border box of an inline-block.
    Range.prototype.getClientRects = function (this: Range) {
      const node = this.commonAncestorContainer;
      if (!(node instanceof Text)) return [line(0, 400)] as unknown as DOMRectList;
      const cell = node.parentElement!.closest<HTMLElement>("td, th")!;
      const first = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT).nextNode();
      const n = node === first ? Number(cell.dataset.lines ?? 1) : 0;
      return Array.from({ length: n }, (_, i) => line(i * 26)) as unknown as DOMRectList;
    };
    box = document.createElement("div");
    document.body.append(box);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    delete (Range.prototype as Partial<Range>).getClientRects;
    document.body.innerHTML = "";
  });

  const OWNER = "| 想了解 | 从这里读 |\n|---|---|\n| Agent 对话怎么跑 | `agent/src/x` |\n| 权限卡片 | `y/z` |";

  function mount(markdown: string, size: { box: number; table: number; lines: number[] }): HTMLElement {
    box.innerHTML = renderMarkdown(markdown);
    return resize(size);
  }
  function resize(size: { box: number; table: number; lines: number[] }): HTMLElement {
    const wrapper = box.querySelector<HTMLElement>(".table-scroll")!;
    Object.defineProperty(wrapper, "clientWidth", { configurable: true, value: size.box });
    Object.defineProperty(wrapper.querySelector("table")!, "offsetWidth", { configurable: true, value: size.table });
    wrapper.querySelectorAll<HTMLElement>("table td, table th").forEach((cell, i) => (cell.dataset.lines = String(size.lines[i] ?? 1)));
    return wrapper;
  }
  /** The engine telling the box's observer that `target` changed size. */
  function notify(target: Element): void {
    const observer = observers[observers.length - 1];
    observer.callback([{ target } as unknown as ResizeObserverEntry], observer as unknown as ResizeObserver);
  }
  const grid = (w: HTMLElement) => w.querySelector("table")!;
  const list = (w: HTMLElement) => w.querySelector<HTMLElement>(".table-list");
  const shown = (el: HTMLElement | null) => el !== null && !el.hidden;

  it("leaves a table that fits as the grid and builds no list", () => {
    const w = mount(OWNER, { box: 520, table: 520, lines: [1, 1, 3, 1, 1, 1] });
    layoutTables(box);
    expect(shown(grid(w))).toBe(true);
    expect(list(w)).toBeNull();
  });

  it("shows the list in place of the grid when a cell needs more than four lines, once the table is whole", () => {
    const w = mount(OWNER, { box: 348, table: 348, lines: [1, 1, 7, 2, 1, 1] });
    layoutTables(box);
    expect(grid(w).hidden).toBe(true);
    expect(shown(list(w))).toBe(true);
    expect(list(w)!.textContent).toContain("想了解: Agent 对话怎么跑");
  });

  it("shows the list when the grid is wider than its box", () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [1, 1, 1, 1, 1, 1] });
    layoutTables(box);
    expect(grid(w).hidden).toBe(true);
  });

  it("does not decide for a table still being written: only blocks up to the last settled one", () => {
    box.innerHTML = renderMarkdown(OWNER) + renderMarkdown("para\n\n" + OWNER);
    const first = box.firstElementChild as HTMLElement;
    const second = box.querySelectorAll<HTMLElement>(".table-scroll")[1];
    for (const w of [first, second]) {
      Object.defineProperty(w, "clientWidth", { configurable: true, value: 348 });
      Object.defineProperty(w.querySelector("table")!, "offsetWidth", { configurable: true, value: 600 });
    }
    layoutTables(box, first);
    expect(grid(first).hidden).toBe(true);
    expect(list(second)).toBeNull();
    expect(grid(second).hidden).toBe(false);
    layoutTables(box, null);
    expect(list(second)).toBeNull();
    layoutTables(box);
    expect(grid(second).hidden).toBe(true);
  });

  it("decides a table once: a later pass with other numbers does not flip it", () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [] });
    layoutTables(box);
    resize({ box: 348, table: 348, lines: [] });
    layoutTables(box);
    expect(grid(w).hidden).toBe(true);
  });

  it("leaves a table whose box has no width yet (not laid out) undecided, and decides it when it gets one", () => {
    const w = mount(OWNER, { box: 0, table: 0, lines: [] });
    layoutTables(box);
    expect(list(w)).toBeNull();
    resize({ box: 348, table: 600, lines: [] });
    notify(w);
    expect(grid(w).hidden).toBe(true);
  });

  it("decides again when the panel is resized, in both directions, and reuses the one list", () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [] });
    layoutTables(box);
    const made = list(w)!;
    const fire = () => notify(w);
    resize({ box: 640, table: 640, lines: [1, 1, 3, 1, 1, 1] });
    fire();
    expect(shown(grid(w))).toBe(true);
    expect(made.hidden).toBe(true);
    resize({ box: 348, table: 348, lines: [1, 1, 6, 1, 1, 1] });
    fire();
    expect(grid(w).hidden).toBe(true);
    expect(list(w)).toBe(made);
    expect(made.hidden).toBe(false);
  });

  it(`ignores a width change under ${RESIZE_SLACK}px, so a scrollbar appearing cannot make the table flip back and forth`, () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [] });
    layoutTables(box);
    resize({ box: 348 + RESIZE_SLACK - 1, table: 300, lines: [] });
    notify(w);
    expect(grid(w).hidden).toBe(true);
  });

  it("stops watching a table that left the page, and when its box is forgotten", () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [] });
    layoutTables(box);
    expect(observers[0].seen.has(w)).toBe(true);
    forgetTables(box);
    expect(observers[0].seen.size).toBe(0);
    expect(grid(w).hidden).toBe(false);
    expect(list(w)).toBeNull();
    layoutTables(box);
    expect(w.querySelectorAll(".table-list")).toHaveLength(1);
    const again = observers[observers.length - 1];
    expect(again.seen.has(w)).toBe(true);
    w.remove();
    notify(w);
    expect(again.seen.has(w)).toBe(false);
  });

  it("counts the lines of the text in a cell, not the one tall box of a wrapped inline-block", () => {
    const w = mount(OWNER, { box: 348, table: 348, lines: [1, 1, 5, 1, 1, 1] });
    layoutTables(box);
    expect(grid(w).hidden).toBe(true);
  });

  it("decides again, in both directions, when the text size changes at the same width", () => {
    const w = mount(OWNER, { box: 520, table: 520, lines: [1, 1, 3, 1, 1, 1] });
    const style = vi.spyOn(window, "getComputedStyle");
    style.mockImplementation(() => ({ fontSize: "15px" }) as CSSStyleDeclaration);
    layoutTables(box);
    expect(shown(grid(w))).toBe(true);
    style.mockImplementation(() => ({ fontSize: "22px" }) as CSSStyleDeclaration);
    resize({ box: 520, table: 700, lines: [1, 1, 6, 1, 1, 1] });
    notify(w);
    expect(grid(w).hidden).toBe(true);
    style.mockImplementation(() => ({ fontSize: "12px" }) as CSSStyleDeclaration);
    resize({ box: 520, table: 520, lines: [1, 1, 2, 1, 1, 1] });
    notify(w);
    expect(shown(grid(w))).toBe(true);
    style.mockRestore();
  });

  it("leaves a table alone on a resize while the reader's selection has an end in it", () => {
    const w = mount(OWNER, { box: 348, table: 600, lines: [] });
    layoutTables(box);
    const cell = list(w)!.querySelector(".table-list-cell")!;
    const selection = getSelection()!;
    selection.setBaseAndExtent(cell.lastChild!, 0, cell.lastChild!, 1);
    resize({ box: 640, table: 640, lines: [] });
    notify(w);
    expect(grid(w).hidden).toBe(true);
    selection.removeAllRanges();
    document.dispatchEvent(new Event("selectionchange"));
    expect(shown(grid(w))).toBe(true);
  });

  it("keeps a table whose cells hold blocks a grid: a copy would show a fence or a diagram twice", () => {
    box.innerHTML = '<div class="table-scroll"><table><thead><tr><th>a</th></tr></thead><tbody><tr><td><pre>x</pre></td></tr></tbody></table></div>';
    const w = box.firstElementChild as HTMLElement;
    Object.defineProperty(w, "clientWidth", { configurable: true, value: 348 });
    Object.defineProperty(w.querySelector("table")!, "offsetWidth", { configurable: true, value: 600 });
    layoutTables(box);
    expect(list(w)).toBeNull();
    expect(grid(w).hidden).toBe(false);
  });

  it("keeps a table with block elements the sanitizer lets through, or a second header, a grid", () => {
    for (const html of [
      "<tbody><tr><td><section>x</section></td></tr></tbody>",
      "<thead><tr><th>a</th></tr></thead><thead><tr><th>b</th></tr></thead><tbody><tr><td>x</td></tr></tbody>",
      "<caption>c</caption><tbody><tr><td>x</td></tr></tbody>",
    ]) {
      box.innerHTML = `<div class="table-scroll"><table>${html}</table></div>`;
      const w = box.firstElementChild as HTMLElement;
      Object.defineProperty(w, "clientWidth", { configurable: true, value: 348 });
      Object.defineProperty(w.querySelector("table")!, "offsetWidth", { configurable: true, value: 600 });
      layoutTables(box);
      expect(list(w), html).toBeNull();
    }
  });

  it("does not take a table a reply nested inside a cell for one of its own", () => {
    box.innerHTML = '<div class="table-scroll"><table><tbody><tr><td><div class="table-scroll"><table><tbody><tr><td>x</td></tr></tbody></table></div></td></tr></tbody></table></div>';
    layoutTables(box);
    expect(observers).toHaveLength(1);
    expect(observers[0].seen.size).toBe(1);
  });

  it("does nothing to a wrapper a reply wrote itself that holds no table, and adds a list only beside a real table", () => {
    box.innerHTML = '<div class="table-scroll"><p>not a table</p></div>';
    const w = box.firstElementChild as HTMLElement;
    Object.defineProperty(w, "clientWidth", { configurable: true, value: 348 });
    layoutTables(box);
    expect(w.innerHTML).toBe("<p>not a table</p>");
  });
});
