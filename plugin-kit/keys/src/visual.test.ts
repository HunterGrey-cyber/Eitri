// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { install } from "./install";
import { seedFind } from "./find";
import { GENERIC_ROWS, type Row } from "./rows";
import type { InstallOptions, VisualOptions } from "./index";
import "./index";

/* jsdom lays nothing out and has no `Selection.modify`. This stand-in moves over the text as one string of
   characters, in document order: a line is a block element's text (a `p`, `li`, `h1`), a word is a run of letters,
   digits and `_`, and a line move keeps the column. It is what WebKit's `modify` does on one-line paragraphs. In a
   table each cell is a block of its own, as in WebKit: a line boundary is the cell's, and a line move goes to the
   next cell of the same row. */
type Place = { node: Text; offset: number; length: number };
function chars(): Place[] {
  const out: Place[] = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const text = n as Text;
    if (text.parentElement?.closest("#eitri-keys-hud")) continue;
    for (let i = 0; i < text.data.length;) {
      const length = text.data.codePointAt(i)! > 0xffff ? 2 : 1;
      out.push({ node: text, offset: i, length });
      i += length;
    }
  }
  return out;
}
const blockOf = (p: Place) => p.node.parentElement!.closest("p, li, h1, h2, pre, div, td, th")!;
function indexOf(all: Place[], node: Node | null, offset: number): number {
  const at = all.findIndex((c) => c.node === node && c.offset === offset);
  if (at >= 0) return at;
  const last = all.map((c, i) => ({ c, i })).filter(({ c }) => c.node === node).pop();
  return last ? last.i + 1 : 0;
}
function placeAt(all: Place[], i: number): [Node, number] {
  if (i < all.length) return [all[i].node, all[i].offset];
  const last = all[all.length - 1];
  return [last.node, last.offset + last.length];
}
function fakeModify(this: Selection, _alter: string, direction: string, granularity: string): void {
  const all = chars();
  let i = indexOf(all, this.focusNode, this.focusOffset);
  const fwd = direction === "forward";
  const word = (k: number) => k >= 0 && k < all.length &&
    /[\p{L}\p{N}_]/u.test(String.fromCodePoint(all[k].node.data.codePointAt(all[k].offset)!));
  const startIn = (of: (p: Place) => Element) => (k: number) => { const b = of(all[Math.min(k, all.length - 1)]); let s = k; while (s > 0 && of(all[s - 1]) === b) s--; return s; };
  const endIn = (of: (p: Place) => Element) => (k: number) => { const b = of(all[Math.min(k, all.length - 1)]); let e = Math.min(k, all.length - 1); while (e < all.length && of(all[e]) === b) e++; return e; };
  const lineStart = startIn(blockOf);
  const lineEnd = endIn(blockOf);
  if (granularity === "character") i = Math.max(0, Math.min(all.length, i + (fwd ? 1 : -1)));
  else if (granularity === "word") {
    if (fwd) { while (i < all.length && !word(i)) i++; while (word(i)) i++; }
    else { while (i > 0 && !word(i - 1)) i--; while (i > 0 && word(i - 1)) i--; }
  } else if (granularity === "lineboundary") {
    // At a line's end the place reads as that line's, as a caret after its last character does.
    const k = fwd || i === 0 ? i : i - 1;
    i = fwd ? lineEnd(Math.min(k, all.length - 1)) : lineStart(k === i ? i : (blockOf(all[k]) === blockOf(all[Math.min(i, all.length - 1)]) ? i : k));
  } else if (granularity === "line") {
    const at = i < all.length ? i : i - 1;
    const start = lineStart(at);
    const col = i - start;
    if (fwd) {
      const next = lineEnd(at);
      if (next < all.length) i = Math.min(next + col, lineEnd(next));
      else i = all.length;
    } else if (start > 0) {
      const prev = lineStart(start - 1);
      i = Math.min(prev + col, start);
    } else i = 0;
  }
  const [node, offset] = placeAt(all, i);
  this.extend(node, offset);
}

let events: Map<string, (data: unknown) => void>;
let cleanup: (() => void) | undefined;
let y = 0;
const pageRows: Row[] = [{ action: "item.next", keys: ["j"], context: "p", desc: "next" }];

function key(key: string, init: KeyboardEventInit = {}) {
  const ev = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
  document.body.dispatchEvent(ev);
  return ev;
}
const sel = () => document.getSelection()!;
const mode = () => document.querySelector<HTMLElement>("#eitri-keys-hud .eitri-keys-mode")?.textContent;
const message = () => document.querySelector("#eitri-keys-hud .eitri-keys-message")?.textContent;

beforeEach(() => {
  events = new Map();
  window.eitriPlugin = {
    call: vi.fn(async (_verb: string, args: unknown) => ({ rows: (args as { rows: Row[] }).rows })),
    on: (name, fn) => { events.set(name, fn); return () => { events.delete(name); }; },
  };
  y = 0;
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => y });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: (q: string) => ({ matches: q.includes("reduce") }) });
  vi.spyOn(window, "scrollTo").mockImplementation(((opts: ScrollToOptions) => { y = opts.top!; }) as typeof window.scrollTo);
  (Selection.prototype as unknown as { modify: typeof fakeModify }).modify = fakeModify;
  // Every character has a box: one line of 20 px per block, in document order.
  const blocks = () => [...document.querySelectorAll("#root p, #root li, #root h1")];
  (Range.prototype as unknown as { getClientRects: () => DOMRect[] }).getClientRects = function (this: Range) {
    const el = (this.startContainer.nodeType === Node.TEXT_NODE ? this.startContainer.parentElement : this.startContainer as Element)
      ?.closest("p, li, h1");
    const at = el ? blocks().indexOf(el) : -1;
    if (at < 0) return [];
    const top = at * 20 - y;
    return [{ top, bottom: top + 20, height: 20, left: 0, right: 10, width: 10, x: 0, y: top, toJSON: () => ({}) } as DOMRect];
  };
  document.body.innerHTML = '<main id="root"><h1>Title here</h1><p>one two three</p><p>four five</p>' +
    "<ul><li>alpha beta</li></ul><p>last words</p></main>";
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  document.body.replaceChildren();
  delete (Selection.prototype as unknown as { modify?: unknown }).modify;
  delete (Range.prototype as unknown as { getClientRects?: unknown }).getClientRects;
  delete (window as unknown as Record<string, unknown>).matchMedia;
  delete (globalThis as Record<string, unknown>).CSS;
  delete (globalThis as Record<string, unknown>).Highlight;
  vi.restoreAllMocks();
});

/** A Custom Highlight API stand-in: the registry, and highlights that keep their ranges. */
function useHighlights(): Map<string, { ranges: Range[] }> {
  const registry = new Map<string, { ranges: Range[] }>();
  (globalThis as Record<string, unknown>).CSS = { highlights: registry };
  (globalThis as Record<string, unknown>).Highlight = class {
    ranges: Range[] = [];
    add(r: Range) { this.ranges.push(r); return this; }
  };
  return registry;
}

/** A range at character `offset` of the text of the `index`th element `selector` matches. */
function at(selector: string, index: number, offset: number): Range {
  const el = document.querySelectorAll(selector)[index];
  const range = document.createRange();
  range.setStart(el.firstChild!, offset);
  return range;
}

async function start(visual?: VisualOptions, run = vi.fn(() => true), extra: Partial<InstallOptions> = {}) {
  cleanup = await install({ rows: pageRows, run, initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" },
    find: { root: () => document.getElementById("root") }, visual, ...extra });
  return run;
}

describe("visual mode's generic rows", () => {
  it("are v and V, after the others, in the host's shared order", () => {
    expect(GENERIC_ROWS.slice(-2).map((r) => [r.action, r.keys])).toEqual([["visual.char", ["v"]], ["visual.line", ["V"]]]);
  });
  it("give way to a page row that takes v", async () => {
    cleanup = await install({ rows: [{ action: "page.v", keys: ["v"], context: "p", desc: "x" }], run: () => true,
      initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" } });
    const declared = ((window.eitriPlugin!.call as ReturnType<typeof vi.fn>).mock.calls[0][1] as { rows: Row[] }).rows;
    expect(declared.some((r) => r.action === "visual.char")).toBe(false);
    expect(declared.find((r) => r.action === "visual.line")?.keys).toEqual(["V"]);
  });
});

describe("v", () => {
  it("starts at the page's cursor with its character selected, and the motions move the cursor as vim's do", async () => {
    const run = await start({ start: () => at("p", 0, 4) });
    key("v");
    expect(mode()).toBe("VISUAL");
    expect(document.documentElement.getAttribute("data-eitri-visual")).toBe("char");
    expect(sel().toString()).toBe("t");
    key("l");
    expect(sel().toString()).toBe("tw");
    key("h");
    key("h");
    // Back past the anchor, vim keeps the anchor's own character.
    expect(sel().toString()).toBe(" t");
    key("e");
    expect(sel().toString()).toBe("two");
    key("w");
    expect(sel().toString()).toBe("two t");
    key("b");
    expect(sel().toString()).toBe("t");
    key("$");
    expect(sel().toString()).toBe("two three");
    key("0");
    expect([sel().toString(), sel().focusOffset]).toEqual(["one t", 0]);
    expect(run).not.toHaveBeenCalled();
  });

  it("keeps the page's own keys away while it runs: j is a line down, not the page's row", async () => {
    const run = await start({ start: () => at("p", 0, 0) });
    key("v");
    key("j");
    expect(sel().toString()).toBe("one two threef");
    expect(run).not.toHaveBeenCalled();
    key("k");
    expect(sel().toString()).toBe("o");
  });

  it("o swaps the ends, and the moving end follows", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    key("e");
    key("o");
    expect([sel().toString(), sel().anchorOffset, sel().focusOffset]).toEqual(["two", 7, 4]);
    key("h");
    expect(sel().toString()).toBe(" two");
  });

  it("with no page cursor starts at the first text in the view", async () => {
    y = 25;
    await start();
    key("v");
    expect(sel().toString()).toBe("o");
    expect(sel().anchorNode!.parentElement!.textContent).toBe("one two three");
  });

  it("keeps a selection the reader made in view", async () => {
    await start({ start: () => at("p", 1, 0) });
    const range = at("p", 0, 0);
    range.setEnd(range.startContainer, 3);
    sel().addRange(range);
    key("v");
    expect(sel().toString()).toBe("one");
  });

  it("says so when there is no text", async () => {
    document.getElementById("root")!.replaceChildren();
    await start();
    key("v");
    expect(mode()).toBe("NORMAL");
    expect(message()).toBe("no text to select");
  });

  it("gg and G stop at the page's first and last text, never the HUD's", async () => {
    await start({ start: () => at("p", 1, 0) });
    key("v");
    key("G", { shiftKey: true });
    expect(sel().toString()).toBe("four fivealpha betalast words");
    expect(sel().focusNode!.textContent).toBe("last words");
    key("g");
    key("g");
    expect(sel().focusNode!.textContent).toBe("Title here");
    expect(sel().focusOffset).toBe(0);
  });

  it("<Esc> and v leave it and drop the selection; the page's keys work again", async () => {
    const run = await start({ start: () => at("p", 0, 0) });
    key("v");
    key("Escape");
    expect([mode(), sel().rangeCount]).toEqual(["NORMAL", 0]);
    expect(document.documentElement.hasAttribute("data-eitri-visual")).toBe(false);
    key("v");
    key("v");
    expect([mode(), sel().rangeCount]).toEqual(["NORMAL", 0]);
    key("j");
    expect(run).toHaveBeenCalledWith("item.next", expect.anything());
  });

  it("ends when the keys leave the page, and at a pointer press", async () => {
    await start({ start: () => at("p", 0, 0) });
    key("v");
    events.get("focus")!({ has_keys: false });
    expect(sel().rangeCount).toBe(0);
    events.get("focus")!({ has_keys: true });
    key("v");
    window.dispatchEvent(new Event("pointerdown"));
    expect(mode()).toBe("NORMAL");
  });

  it("n and N extend the focus to the start of the next and previous match of the last search", async () => {
    // Find searches drawn elements only; every element here has a box.
    vi.spyOn(Element.prototype, "getClientRects").mockImplementation(() =>
      [{ top: 0, bottom: 20, height: 20, left: 0, right: 10, width: 10 }] as unknown as DOMRectList);
    await start({ start: () => at("p", 0, 0) });
    seedFind("e");
    key("v");
    key("n");
    expect(sel().toString()).toBe("one");
    key("n");
    expect(sel().toString()).toBe("one two thre");
    key("N", { shiftKey: true });
    expect(sel().toString()).toBe("one");
  });
});

describe("text the mode stands on", () => {
  it("w and e read a letter outside the BMP as one word character", async () => {
    document.getElementById("root")!.innerHTML = "<p>a \u{10400} b</p>";
    await start({ start: () => at("p", 0, 0) });
    key("v");
    key("w");
    expect(sel().toString()).toBe("a \u{10400}");
  });

  it("is ended by visualMode() when the page replaced the text under it, HUD and root mark too", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    const p0 = document.querySelector("p")!;
    p0.firstChild!.replaceWith(document.createTextNode("one two three"));
    expect(window.eitriKeys.visualMode()).toBe(null);
    expect(mode()).toBe("NORMAL");
    expect(document.documentElement.hasAttribute("data-eitri-visual")).toBe(false);
  });

  it("ends the mode when an update shrinks the text under it", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    key("l");
    (document.querySelector("p")!.firstChild as Text).data = "x";
    vi.spyOn(console, "warn").mockImplementation(() => {});
    key("l");
    expect(mode()).toBe("NORMAL");
  });
});

describe("} and {", () => {
  it("v} takes the selection to its block's end, then the next block's; { back to a block's start", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    key("}");
    expect(sel().toString()).toBe("two three");
    key("}");
    expect(sel().toString()).toBe("two threefour five");
    key("{");
    expect(sel().toString()).toBe("two threef");
    key("{");
    expect(sel().toString()).toBe("one t");
    key("{");
    expect(sel().toString()).toBe("Title hereone t");
  });

  // Linewise the same motions take whole lines (`wholeLines`); the stand-in's line boundary is not WebKit's at a
  // block's end, so this reads the motion charwise and the sandbox pass shows `V}`.
  it("a list item's own text beside its nested list and a table row are blocks of their own", async () => {
    document.body.innerHTML = '<main id="root"><p>intro text</p><ul><li>own text<ul><li>nested</li></ul></li></ul>' +
      "<table><tbody><tr><td>core</td><td>146</td></tr><tr><td>shell</td><td>48</td></tr></tbody></table></main>";
    await start({ start: () => at("p", 0, 0) });
    key("v");
    key("}");
    expect(sel().toString()).toBe("intro text");
    key("}");
    expect(sel().toString()).toBe("intro textown text");
    key("}");
    expect(sel().toString()).toBe("intro textown textnested");
    key("}");
    expect(sel().toString()).toBe("intro textown textnestedcore146");
    key("{");
    expect(sel().toString()).toBe("intro textown textnestedc");
  });
});

describe("V", () => {
  it("selects the cursor's whole line, and j and k add and drop whole lines", async () => {
    await start({ start: () => at("p", 0, 5) });
    key("V", { shiftKey: true });
    expect(mode()).toBe("VISUAL LINE");
    expect(document.documentElement.getAttribute("data-eitri-visual")).toBe("line");
    expect(sel().toString()).toBe("one two three");
    key("j");
    expect(sel().toString()).toBe("one two threefour five");
    key("k");
    expect(sel().toString()).toBe("one two three");
    key("v");
    expect(mode()).toBe("VISUAL");
    key("V", { shiftKey: true });
    expect(mode()).toBe("VISUAL LINE");
    key("V", { shiftKey: true });
    expect(mode()).toBe("NORMAL");
  });
});

describe("in a table", () => {
  const table = () => {
    document.body.innerHTML = '<main id="root"><p>before</p><table><thead><tr><th>crate</th><th>lines</th>' +
      "<th>tests</th></tr></thead><tbody><tr><td>core</td><td>146,507</td><td>244,246</td></tr>" +
      "<tr><td>shell</td><td>48,315</td><td>57,077</td></tr></tbody></table><p>after</p></main>";
  };

  it("V takes the cursor's whole row, every cell, from any cell; j adds the next row whole", async () => {
    table();
    await start({ start: () => at("td", 1, 2) });
    key("V", { shiftKey: true });
    expect(sel().toString()).toBe("core146,507244,246");
    key("j");
    expect(sel().toString()).toBe("core146,507244,246shell48,31557,077");
    key("k");
    key("k");
    expect(sel().toString()).toBe("cratelinestestscore146,507244,246");
  });

  it("j and k go a row, to the same column and character; past the table's edge, to the text beyond it", async () => {
    table();
    await start({ start: () => at("td", 1, 2) });
    key("v");
    key("j");
    expect(sel().toString()).toBe("6,507244,246shell48,");
    key("j");
    expect(sel().focusNode?.parentElement?.tagName).toBe("P");
    expect(sel().toString().endsWith("a")).toBe(true);
    key("k");
    key("k");
    key("k");
    expect(sel().toString()).toBe("testscore146");
    key("k");
    expect(sel().toString()).toBe("ecratelinestestscore146");
  });

  it("k and j count characters in code points: never into the middle of a letter outside the BMP", async () => {
    document.body.innerHTML = '<main id="root"><table><tbody><tr><td>\u{1F600}x</td></tr><tr><td>abcd</td></tr>' +
      "<tr><td>\u{1F600}</td></tr></tbody></table></main>";
    await start({ start: () => at("td", 1, 1) });
    key("v");
    key("k");
    expect(sel().toString()).toBe("xab");
    key("Escape");
    cleanup?.();
    await start({ start: () => at("td", 1, 3) });
    key("v");
    key("j");
    expect(sel().toString()).toBe("d\u{1F600}");
  });

  it("v selects exactly the characters, across cells too", async () => {
    table();
    await start({ start: () => at("td", 3, 0) });
    key("v");
    key("$");
    expect(sel().toString()).toBe("shell");
    key("l");
    key("$");
    expect(sel().toString().replace(/\s+/g, "")).toBe("shell48,315");
  });
});

describe("painting", () => {
  it("draws the selection as the eitri-visual highlight, not the engine's selection, and drops it at the end", async () => {
    const registry = useHighlights();
    await start({ start: () => at("p", 0, 4) });
    key("v");
    key("e");
    const root = document.documentElement;
    expect(root.getAttribute("data-eitri-visual-paint")).toBe("highlight");
    expect(registry.get("eitri-visual")?.ranges.map((r) => r.toString())).toEqual(["two"]);
    key("j");
    expect(registry.get("eitri-visual")?.ranges.map((r) => r.toString())).toEqual([sel().toString()]);
    key("V", { shiftKey: true });
    expect(registry.get("eitri-visual")?.ranges.map((r) => r.toString())).toEqual(["one two threefour five"]);
    key("Escape");
    expect(registry.has("eitri-visual")).toBe(false);
    expect(root.hasAttribute("data-eitri-visual-paint")).toBe(false);
  });

  it("without the Custom Highlight API leaves the engine's selection to draw it", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    expect(document.documentElement.hasAttribute("data-eitri-visual-paint")).toBe(false);
    expect(sel().toString()).toBe("t");
  });

  it("the stylesheet hides the engine's selection only while the kit paints", async () => {
    const { STYLE_TEXT } = await import("./style");
    expect(STYLE_TEXT).toContain("html[data-eitri-visual-paint] ::selection{background-color:transparent}");
    expect(STYLE_TEXT).toContain("::highlight(eitri-visual){background-color:var(--nv-cursorline, #4f5258)}");
    expect(STYLE_TEXT.indexOf("html[data-eitri-visual-paint] ::selection"))
      .toBeGreaterThan(STYLE_TEXT.indexOf("html[data-eitri-visual] ::selection"));
  });
});

describe("y", () => {
  it("copies the selection as shown with the copy command, says how much, and leaves", async () => {
    const exec = vi.fn(() => true);
    (document as unknown as { execCommand: typeof exec }).execCommand = exec;
    await start({ start: () => at("p", 0, 0) });
    key("v");
    key("e");
    let copied = "";
    exec.mockImplementation(() => { copied = sel().toString(); return true; });
    key("y");
    expect(exec).toHaveBeenCalledWith("copy");
    expect(copied).toBe("one");
    expect([mode(), message(), sel().rangeCount]).toEqual(["NORMAL", "3 characters yanked", 0]);
    delete (document as unknown as { execCommand?: unknown }).execCommand;
  });

  it("falls back to the clipboard API, and says when neither worked", async () => {
    const writes: string[] = [];
    Object.defineProperty(navigator, "clipboard", { configurable: true,
      value: { writeText: async (t: string) => { writes.push(t); } } });
    await start({ start: () => at("p", 0, 0) });
    key("v");
    key("y");
    await Promise.resolve();
    await Promise.resolve();
    expect([writes, message()]).toEqual([["o"], "1 character yanked"]);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async () => { throw new Error("no"); } } });
    key("v");
    key("y");
    await Promise.resolve();
    await Promise.resolve();
    expect(message()).toBe("could not copy to the clipboard");
    delete (navigator as unknown as { clipboard?: unknown }).clipboard;
  });
});

describe("a page's visual hooks", () => {
  it("follow gets the moving end's box after every move, and the kit then scrolls nothing", async () => {
    const boxes: number[] = [];
    await start({ start: () => at("p", 0, 0), follow: (rect) => { boxes.push(rect.top); } });
    key("v");
    key("j");
    key("j");
    expect(boxes).toEqual([20, 40, 60]);
    expect(window.scrollTo).not.toHaveBeenCalled();
  });

  it("without follow, the kit keeps the moving end in view", async () => {
    Object.defineProperty(window, "innerHeight", { configurable: true, value: 60 });
    await start({ start: () => at("p", 0, 0) });
    key("v");
    key("G", { shiftKey: true });
    expect(y).toBeGreaterThan(0);
    delete (window as unknown as Record<string, unknown>).innerHeight;
  });

  it("a page key runs with the kind and the selection, and ends the mode when it took the key", async () => {
    const seen: string[] = [];
    const enter = vi.fn((kind: string) => { seen.push(`${kind}:${sel().toString()}`); return true; });
    const big = vi.fn(() => false);
    await start({ start: () => at("p", 0, 0), keys: { "<CR>": enter, Y: big } });
    key("v");
    key("e");
    key("Y", { shiftKey: true });
    expect(mode()).toBe("VISUAL");
    key("Enter");
    expect(seen).toEqual(["char:one"]);
    expect(mode()).toBe("NORMAL");
  });
});
