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
   next cell of the same row. An image or a checkbox is one step with no text, as both engines step over it (the
   selection's text gains nothing, and the place before it is the element place in its parent). */
type Place = { node: Node; offset: number; length: number; atom?: boolean };
const ATOM = "img, input";
/** With `breaks` on, a `<br>` is a step of its own too (the engines step over a line break as over a character); a
 *  test that turns it on makes the selection's text gain the line break. */
let breaks = false;
function chars(): Place[] {
  const out: Place[] = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const el = n.nodeType === Node.ELEMENT_NODE ? n as Element : n.parentElement;
    if (el?.closest("#eitri-keys-hud")) continue;
    if (n.nodeType === Node.ELEMENT_NODE) {
      if ((n as Element).matches(ATOM) || (breaks && (n as Element).matches("br"))) {
        out.push({ node: n, offset: 0, length: 0, atom: true });
      }
      continue;
    }
    const text = n as Text;
    for (let i = 0; i < text.data.length;) {
      const length = text.data.codePointAt(i)! > 0xffff ? 2 : 1;
      out.push({ node: text, offset: i, length });
      i += length;
    }
  }
  return out;
}
const blockOf = (p: Place) => (p.atom ? p.node as Element : p.node.parentElement!).closest("p, li, h1, h2, pre, div, td, th")!;
/** The boundary point just before a step: a character's text and offset, an atom's place in its parent. */
const pointOf = (c: Place): [Node, number] =>
  c.atom ? [c.node.parentNode!, [...c.node.parentNode!.childNodes].indexOf(c.node as ChildNode)] : [c.node, c.offset];
/** The first step at or after a place, in document order (a place after the last one: their count). */
function indexOf(all: Place[], node: Node | null, offset: number): number {
  if (!node) return 0;
  const exact = all.findIndex((c) => !c.atom && c.node === node && c.offset === offset);
  if (exact >= 0) return exact;
  const range = document.createRange();
  range.setStart(node, offset);
  const after = all.findIndex((c) => range.comparePoint(...pointOf(c)) >= 0);
  return after >= 0 ? after : all.length;
}
/** Where the engine reports a place that is both the end of one text and the start of the next: WebKit and Blink
 *  canonicalise it upstream, as the end of the text before (`text`) or as an element place before the next text
 *  (`element`); the default stand-in reports the start of the next text. A place after an atom is the element place
 *  after it in every mode, as both engines report it. */
let upstream: "off" | "text" | "element" = "off";
/** ICU's dictionary word breaks inside a Chinese run (`中文|中文`): the stand-in's word granularity breaks one every
 *  second ideograph, as the engine does. The kit walks `w`, `b` and `e` itself, so a run stays one word for it. */
let cjkSplit = false;
/** Where a backward character move does not move at all (a selector): inside the element, or at the place just
 *  before it. Blink's step back stops at a zero-width space inside KaTeX's glyphs, and at the place between the MathML
 *  copy and the glyphs. */
let stuck: string | null = null;
/** Elements the engines will not step back into or past from the place right after them (a selector): both do so
 *  after an `<hr>` or a table that is a child of the root. A character step forward over such an element's end lands
 *  on that place with no text, as theirs does, and so does a step back from the first character after it. */
let walls: string | null = null;
function wallStep(s: Selection, all: Place[], i: number, fwd: boolean): boolean {
  for (const wall of document.querySelectorAll(walls!)) {
    const parent = wall.parentNode!;
    const offset = [...parent.childNodes].indexOf(wall as ChildNode) + 1;
    const after = document.createRange();
    after.setStart(parent, offset);
    const focus = after.comparePoint(s.focusNode!, s.focusOffset);
    const next = all[fwd ? i : i - 1];
    const side = next ? after.comparePoint(...pointOf(next)) : -1;
    if (!fwd && focus === 0) return true;
    if (fwd ? focus < 0 && side >= 0 : focus > 0 && side < 0) {
      s.extend(parent, offset);
      return true;
    }
    // The last character before the wall: the step ends at its end, in its own text.
    const land = all[i + 1];
    if (fwd && next && !next.atom && focus < 0 && side < 0 && land && after.comparePoint(...pointOf(land)) > 0) {
      s.extend(next.node, next.offset + next.length);
      return true;
    }
  }
  return false;
}
function placeAt(all: Place[], i: number): [Node, number] {
  const before = all[i - 1];
  if (before?.atom) {
    const [parent, index] = pointOf(before);
    return [parent, index + 1];
  }
  if (i < all.length) {
    const c = all[i];
    if (upstream !== "off" && i > 0 && c.offset === 0 && before.node !== c.node) {
      if (upstream === "text") return [before.node, before.offset + before.length];
      if (!c.atom) {
        const parent = c.node.parentElement!;
        return [parent, [...parent.childNodes].indexOf(c.node as ChildNode)];
      }
    }
    return pointOf(c);
  }
  const last = all[all.length - 1];
  return [last.node, last.offset + last.length];
}
const textOf = (c: Place | undefined) => (c && !c.atom ? (c.node as Text).data : "");
function stuckAt(node: Node | null, offset: number, selector: string): boolean {
  if (!node) return false;
  if (node.nodeType === Node.TEXT_NODE) return !!node.parentElement?.closest(selector);
  const next = node.childNodes[offset];
  return !!(node as Element).closest(selector) ||
    (next?.nodeType === Node.ELEMENT_NODE && (next as Element).matches(selector));
}
function fakeModify(this: Selection, _alter: string, direction: string, granularity: string): void {
  const all = chars();
  let i = indexOf(all, this.focusNode, this.focusOffset);
  const fwd = direction === "forward";
  if (stuck && !fwd && granularity === "character" && stuckAt(this.focusNode, this.focusOffset, stuck)) return;
  if (walls && granularity === "character" && wallStep(this, all, i, fwd)) return;
  const word = (k: number) => k >= 0 && k < all.length &&
    !all[k].atom && /[\p{L}\p{N}_]/u.test(String.fromCodePoint(textOf(all[k]).codePointAt(all[k].offset)!));
  const startIn = (of: (p: Place) => Element) => (k: number) => { const b = of(all[Math.min(k, all.length - 1)]); let s = k; while (s > 0 && of(all[s - 1]) === b) s--; return s; };
  const endIn = (of: (p: Place) => Element) => (k: number) => { const b = of(all[Math.min(k, all.length - 1)]); let e = Math.min(k, all.length - 1); while (e < all.length && of(all[e]) === b) e++; return e; };
  const lineStart = startIn(blockOf);
  const lineEnd = endIn(blockOf);
  if (granularity === "character") i = Math.max(0, Math.min(all.length, i + (fwd ? 1 : -1)));
  else if (granularity === "word") {
    if (fwd) { while (i < all.length && !word(i)) i++; while (word(i)) i++; }
    else {
      while (i > 0 && !word(i - 1)) i--;
      while (i > 0 && word(i - 1)) {
        i--;
        if (cjkSplit && /\p{Script=Han}/u.test(textOf(all[i])[all[i].offset] ?? "") && i % 2 === 0) break;
      }
    }
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
  upstream = "off";
  cjkSplit = false;
  breaks = false;
  stuck = null;
  walls = null;
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

  it("follow_cursor: follow gets the cursor's place too, in v and V, not the selection's focus", async () => {
    type Spot = { node: Node; offset: number };
    const seen: (Spot | undefined)[] = [];
    const spot = (selector: string, index: number, offset: number): Spot =>
      ({ node: document.querySelectorAll(selector)[index].firstChild!, offset });
    // Every place has a box here, a table cell's too (the stand-in's boxes cover paragraphs only).
    const box = { top: 0, bottom: 20, height: 20, left: 0, right: 10, width: 10, x: 0, y: 0, toJSON: () => ({}) };
    (Range.prototype as unknown as { getClientRects: () => DOMRect[] }).getClientRects = () => [box as DOMRect];
    // A line long enough to wrap in a window: the stand-in has no wrapping, so a line is the paragraph.
    document.getElementById("root")!.innerHTML = "<p>one two three four five six seven eight nine ten</p>" +
      "<p>four five</p><table><tbody><tr><td>core</td><td>146,507</td><td>244,246</td></tr>" +
      "<tr><td>shell</td><td>48,315</td><td>57,077</td></tr></tbody></table><p>after</p>";
    await start({ start: () => at("p", 0, 4), follow: (_rect, cursor) => { seen.push(cursor); } });
    key("v");
    key("l");
    expect(seen).toEqual([spot("p", 0, 4), spot("p", 0, 5)]);
    key("V", { shiftKey: true });
    // Linewise, the selection's focus is the line's end; the cursor is where it was.
    expect(sel().focusNode === spot("p", 0, 0).node && sel().focusOffset === 5).toBe(false);
    expect(seen.at(-1)).toEqual(spot("p", 0, 5));
    key("j");
    expect(seen.at(-1)).toEqual(spot("p", 1, 5));
    key("Escape");
    // A table row is one line under V: the selection runs to the row's last cell, the cursor stays in its own.
    seen.length = 0;
    cleanup?.();
    await start({ start: () => at("td", 1, 2), follow: (_rect, cursor) => { seen.push(cursor); } });
    key("V", { shiftKey: true });
    expect(sel().toString()).toBe("core146,507244,246");
    expect(seen).toEqual([spot("td", 1, 2)]);
    key("j");
    expect(sel().focusNode).toBe(spot("td", 5, 0).node);
    expect(seen.at(-1)).toEqual(spot("td", 4, 2));
  });

  it("follow_cursor_shape: the cursor's place is motion's, the character's text, not the end of the text before",
    async () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<p>see <code>foo</code> bar</p>";
      const see = document.querySelector("p")!.firstChild!;
      const foo = document.querySelector("code")!.firstChild!;
      let cursor: { node: Node; offset: number } | undefined;
      await start({ start: () => { const r = document.createRange(); r.setStart(see, 4); return r; },
        follow: (_rect, c) => { cursor = c; } });
      key("v");
      expect(cursor).toEqual({ node: foo, offset: 0 });
      key("l");
      expect(cursor).toEqual({ node: foo, offset: 1 });
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

describe("motion (kit 6): the walker as a Normal-mode motion", () => {
  type Spot = { node: Node; offset: number };
  const motion = (from: Spot, m: string) =>
    (window.eitriKeys.motion as (at: Spot, m: string) => Spot | null)(from, m);
  const spot = (selector: string, index: number, offset: number): Spot => {
    const r = at(selector, index, offset);
    return { node: r.startContainer, offset: r.startOffset };
  };
  /** The place of visual mode's cursor: just before its character, read from the selection as `v` reads one. */
  const visualCursor = (): Spot => {
    const s = sel();
    const range = document.createRange();
    range.setStart(s.anchorNode!, s.anchorOffset);
    const forward = range.comparePoint(s.focusNode!, s.focusOffset) >= 0;
    if (!forward) return { node: s.focusNode!, offset: s.focusOffset };
    if (s.focusOffset > 0) return { node: s.focusNode!, offset: s.focusOffset - 1 };
    // The stand-in reports a line's end as the next line's start: the character is the previous text's last.
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    walker.currentNode = s.focusNode!;
    const before = walker.previousNode() as Text;
    return { node: before, offset: before.data.length - 1 };
  };
  const ends = () => [sel().anchorNode, sel().anchorOffset, sel().focusNode, sel().focusOffset];

  it("motion_matches_visual_mode: h l w b e 0 $ reach the place visual mode's cursor reaches", async () => {
    for (const offset of [4, 5, 8]) {
      for (const k of ["h", "l", "w", "b", "e", "0", "$"]) {
        const from = spot("p", 0, offset);
        await start({ start: () => at("p", 0, offset) });
        const got = motion(from, k);
        key("v");
        key(k);
        const want = visualCursor();
        key("Escape");
        cleanup?.();
        cleanup = undefined;
        expect(got, `${k} from ${offset}`).toEqual(want);
      }
    }
  });

  it("walks from a place with no page installed, over the whole document", () => {
    expect(motion(spot("p", 0, 4), "w")).toEqual(spot("p", 0, 8));
    expect(motion(spot("p", 0, 4), "$")).toEqual(spot("p", 0, 12));
  });

  it("motion_keeps_the_selection: a forward, a backward and no selection come back as they were", async () => {
    await start();
    const cases: Array<[string, [number, number] | null]> = [["forward", [0, 3]], ["backward", [3, 0]],
      ["collapsed", [2, 2]], ["none", null]];
    for (const [name, span] of cases) {
      sel().removeAllRanges();
      const p = document.querySelector("p")!.firstChild!;
      if (span) sel().setBaseAndExtent(p, span[0], p, span[1]);
      const before = ends();
      for (const k of ["h", "l", "w", "b", "e", "0", "$"]) {
        expect(motion(spot("p", 1, 2), k), `${name} ${k}`).not.toBeNull();
        expect(ends(), `${name} ${k}`).toEqual(before);
      }
      expect(sel().rangeCount, name).toBe(span ? 1 : 0);
    }
  });

  it("keeps a backward selection on every null path: before the walk, after it, and when the walk throws", async () => {
    await start();
    const p = document.querySelector("p")!.firstChild!;
    sel().setBaseAndExtent(p, 5, p, 1);
    document.body.insertAdjacentHTML("beforeend", "<p id=after>after</p>");
    const before = ends();
    // Before the walk: a start that is not the page's, and a key that is not a Normal-mode motion.
    expect(motion({ node: document.createTextNode("loose"), offset: 0 }, "l")).toBeNull();
    expect(motion(spot("p", 1, 2), "j")).toBeNull();
    expect(ends()).toEqual(before);
    // After the walk: the answer is off the page, though the walk moved the selection.
    const last = document.querySelectorAll("#root p")[2].firstChild as Text;
    expect(motion({ node: last, offset: last.data.length }, "l")).toBeNull();
    expect(ends()).toEqual(before);
    // The walk throws half way: the selection was moved first.
    const modify = Selection.prototype.modify as unknown as (this: Selection, ...a: string[]) => void;
    (Selection.prototype as unknown as { modify: unknown }).modify = function (this: Selection, ...a: string[]) {
      modify.apply(this, a);
      throw new Error("engine");
    };
    expect(motion(spot("p", 1, 2), "l")).toBeNull();
    expect(ends()).toEqual(before);
  });

  it("answers null, and touches no selection, while a field or an editable element has the focus", async () => {
    document.getElementById("root")!.insertAdjacentHTML("beforeend",
      '<input id=field value="hello world"><div id=edit contenteditable tabindex=0>typed</div>');
    await start();
    const field = document.getElementById("field") as HTMLInputElement;
    field.focus();
    field.setSelectionRange(2, 5);
    const before = ends();
    expect(motion(spot("p", 1, 2), "l")).toBeNull();
    expect([field.selectionStart, field.selectionEnd]).toEqual([2, 5]);
    expect(ends()).toEqual(before);
    document.getElementById("edit")!.focus();
    expect(motion(spot("p", 1, 2), "l")).toBeNull();
    field.blur();
    (document.activeElement as HTMLElement | null)?.blur();
    expect(motion(spot("p", 1, 2), "l")).toEqual(spot("p", 1, 3));
  });

  it("resolves the page's root once, before the selection is borrowed", async () => {
    document.getElementById("root")!.innerHTML = "<section id=one><p>aaa</p></section><section id=two><p>bbb</p></section>";
    // A root that follows the selection's focus: during the walk the focus is wherever the walk has got to.
    const root = () => document.getSelection()?.focusNode?.parentElement?.closest("section") ?? document.getElementById("one");
    cleanup = await install({ rows: pageRows, run: () => true, initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" },
      find: { root } });
    const text = document.querySelector("#one p")!.firstChild as Text;
    sel().setBaseAndExtent(text, 0, text, 1);
    expect(motion({ node: text, offset: 1 }, "l")).toEqual({ node: text, offset: 2 });
    // From the first section's last character one step on is the second section's: off the page for the root the
    // page has before the walk, though the walk's own focus is in the second section.
    expect(motion({ node: text, offset: 2 }, "l")).toBeNull();
  });

  it("never throws or leaves the walker's selection when the borrowed one cannot be put back", async () => {
    await start();
    const p = document.querySelector("p")!.firstChild!;
    sel().setBaseAndExtent(p, 5, p, 1);
    vi.spyOn(Selection.prototype, "setBaseAndExtent").mockImplementation(() => { throw new DOMException("gone", "IndexSizeError"); });
    expect(motion(spot("p", 1, 2), "l")).toEqual(spot("p", 1, 3));
    expect(sel().rangeCount).toBe(0);
    vi.restoreAllMocks();
    sel().setBaseAndExtent(p, 1, p, 5);
    vi.spyOn(Selection.prototype, "addRange").mockImplementation(() => { throw new DOMException("gone", "IndexSizeError"); });
    expect(motion(spot("p", 1, 2), "l")).toEqual(spot("p", 1, 3));
    expect(sel().rangeCount).toBe(0);
  });

  it("puts a forward selection back as the very Range it was, a backward one by its ends", async () => {
    await start();
    const p = document.querySelector("p")!.firstChild!;
    sel().setBaseAndExtent(p, 1, p, 5);
    const held = sel().getRangeAt(0);
    expect(motion(spot("p", 1, 2), "w")).not.toBeNull();
    expect(sel().getRangeAt(0)).toBe(held);
    expect([held.startOffset, held.endOffset]).toEqual([1, 5]);
    sel().setBaseAndExtent(p, 5, p, 1);
    expect(motion(spot("p", 1, 2), "w")).not.toBeNull();
    expect(ends()).toEqual([p, 5, p, 1]);
  });

  it("answers null, not a TypeError, before the document has a body", () => {
    document.head.innerHTML = "<title>a title</title>";
    const title = document.querySelector("title")!.firstChild!;
    const body = document.body;
    body.remove();
    try {
      expect(motion({ node: title, offset: 1 }, "l")).toBeNull();
    } finally {
      document.documentElement.append(body);
    }
  });

  describe("the place is just before the character the cursor is on", () => {
    const KEYS = ["h", "l", "w", "b", "e", "0", "$"];
    /** A text place that is not the end of its text, unless no character follows it on its line. */
    const isBeforeCharacter = (r: Spot | null) => {
      expect(r).not.toBeNull();
      const node = r!.node as Text;
      expect(node.nodeType).toBe(Node.TEXT_NODE);
      if (r!.offset === node.data.length) {
        // The end of a text is allowed only where no text follows it in its paragraph.
        const walker = document.createTreeWalker(node.parentElement!.closest("p")!, NodeFilter.SHOW_TEXT);
        walker.currentNode = node;
        expect(walker.nextNode(), "no character follows").toBeNull();
      }
    };

    it("motion_inline_boundary: l and w across an inline element land on its first character, e and $ on the last", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<p>see <code>foo</code> bar</p>";
      const walker = document.createTreeWalker(document.querySelector("p")!, NodeFilter.SHOW_TEXT);
      const [see, foo, bar] = [walker.nextNode(), walker.nextNode(), walker.nextNode()] as Text[];
      expect(motion({ node: foo, offset: 2 }, "l")).toEqual({ node: bar, offset: 0 });
      expect(motion({ node: see, offset: 0 }, "w")).toEqual({ node: foo, offset: 0 });
      expect(motion({ node: see, offset: 2 }, "l")).toEqual({ node: see, offset: 3 });
      expect(motion({ node: see, offset: 3 }, "l")).toEqual({ node: foo, offset: 0 });
      // The cursor is on the last character: the place before it.
      expect(motion({ node: see, offset: 0 }, "e")).toEqual({ node: see, offset: 2 });
      expect(motion({ node: see, offset: 3 }, "e")).toEqual({ node: foo, offset: 2 });
      expect(motion({ node: foo, offset: 0 }, "$")).toEqual({ node: bar, offset: 3 });
      expect(motion({ node: bar, offset: 1 }, "0")).toEqual({ node: see, offset: 0 });
      for (const from of [see, foo, bar]) {
        for (let offset = 0; offset < from.data.length; offset++) {
          for (const k of KEYS) isBeforeCharacter(motion({ node: from, offset }, k));
        }
      }
    });

    it("motion_element_place: a place the engine gives as an element before the next text is that text's start", () => {
      upstream = "element";
      document.getElementById("root")!.innerHTML = "<p>see <code>foo</code> bar</p>";
      const bar = document.querySelector("p")!.lastChild as Text;
      const foo = document.querySelector("code")!.firstChild as Text;
      expect(motion({ node: foo, offset: 2 }, "l")).toEqual({ node: bar, offset: 0 });
    });

    it("keeps the end of a line as it is: no character follows it on its line", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<p>a</p><p>b</p>";
      const a = document.querySelector("p")!.firstChild as Text;
      expect(motion({ node: a, offset: 0 }, "l")).toEqual({ node: a, offset: 1 });
    });

    it("keeps the end of a line that a line break ends", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<p>one<br>two</p>";
      // The engine's selection text has a line break where the break lies.
      vi.spyOn(Selection.prototype, "toString").mockImplementation(function (this: Selection) {
        if (this.rangeCount === 0) return "";
        const range = this.getRangeAt(0);
        return range.toString() + (range.cloneContents().querySelector("br") ? "\n" : "");
      });
      const one = document.querySelector("p")!.firstChild as Text;
      expect(motion({ node: one, offset: 2 }, "l")).toEqual({ node: one, offset: 3 });
    });
  });

  describe("an inline atom (an image, a checkbox) is no character", () => {
    const texts = () => {
      const walker = document.createTreeWalker(document.getElementById("root")!, NodeFilter.SHOW_TEXT);
      const out: Text[] = [];
      for (let n = walker.nextNode(); n; n = walker.nextNode()) out.push(n as Text);
      return out;
    };

    it("motion_inline_atom: h goes back past an image to the character before it, l and w on to the one after", () => {
      for (const mode of ["off", "text", "element"] as const) {
        upstream = mode;
        document.getElementById("root")!.innerHTML = "<p>foo<img>bar</p>";
        const [foo, bar] = texts();
        expect(motion({ node: bar, offset: 0 }, "h"), mode).toEqual({ node: foo, offset: 2 });
        expect(motion({ node: foo, offset: 2 }, "l"), mode).toEqual({ node: bar, offset: 0 });
        expect(motion({ node: foo, offset: 0 }, "w"), mode).toEqual({ node: bar, offset: 0 });
        expect(motion({ node: bar, offset: 0 }, "b"), mode).toEqual({ node: foo, offset: 0 });
        expect(motion({ node: foo, offset: 2 }, "e"), mode).toEqual({ node: bar, offset: 2 });
        // h all the way back crosses the image once and never sticks.
        const seen: Array<Spot | null> = [];
        for (let p: Spot | null = { node: bar, offset: 2 }, i = 0; i < 5; i++) seen.push(p = p && motion(p, "h"));
        expect(seen, mode).toEqual([{ node: bar, offset: 1 }, { node: bar, offset: 0 }, { node: foo, offset: 2 },
          { node: foo, offset: 1 }, { node: foo, offset: 0 }]);
      }
    });

    it("$ and e step back past an image that ends the line onto its last character", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<p>foo bar<img></p><p>next</p>";
      const [text] = texts();
      expect(motion({ node: text, offset: 0 }, "$")).toEqual({ node: text, offset: 6 });
      expect(motion({ node: text, offset: 4 }, "e")).toEqual({ node: text, offset: 6 });
    });

    it("motion_b_across_checkbox: b passes a checkbox as it passes a space, to the word before it", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = "<ul><li>see <input type=checkbox> done</li></ul>";
      const [see, done] = texts();
      expect(motion({ node: done, offset: 1 }, "b")).toEqual({ node: see, offset: 0 });
      expect(motion({ node: done, offset: 0 }, "b")).toEqual({ node: see, offset: 0 });
      expect(motion({ node: see, offset: 0 }, "w")).toEqual({ node: done, offset: 1 });
    });

    it("visual mode's b passes a checkbox too", async () => {
      document.getElementById("root")!.innerHTML = "<ul><li>see <input type=checkbox> done</li></ul>";
      const done = texts()[1];
      await start({ start: () => { const r = document.createRange(); r.setStart(done, 1); return r; } });
      key("v");
      key("b");
      expect(sel().toString()).toBe("see  d");
    });

    it("l from the root's last character stops at the root's end, without walking the text after the root", async () => {
      upstream = "text";
      document.body.insertAdjacentHTML("beforeend", `<p id=after>${"after ".repeat(50)}</p>`);
      await start();
      const last = document.querySelectorAll("#root p")[2].firstChild as Text;
      const modify = vi.spyOn(Selection.prototype as unknown as { modify: typeof fakeModify }, "modify");
      expect(motion({ node: last, offset: last.data.length - 1 }, "l")).toEqual({ node: last, offset: last.data.length });
      expect(modify.mock.calls.length).toBeLessThan(10);
    });

    it("motion_hidden_inline: l from the text before an aria-hidden inline lands on the text after it", () => {
      upstream = "text";
      document.getElementById("root")!.innerHTML = '<p>foo<span aria-hidden="true">X</span>bar</p>';
      const [foo, , bar] = texts();
      expect(motion({ node: foo, offset: 2 }, "l")).toEqual({ node: bar, offset: 0 });
    });
  });

  describe("text the page leaves out (KaTeX's hidden MathML copy and its aria-hidden glyphs) is no character", () => {
    /** Inline math as KaTeX renders it (`htmlAndMathml`): a MathML copy the page's stylesheet clips to a pixel, then
     *  the drawn glyphs in `aria-hidden` markup. The stand-in steps through both, as WebKit does. */
    const katex = (glyphs: string) => '<span class="katex"><span class="katex-mathml"><math><semantics><mrow>' +
      [...glyphs].map((g) => `<mi>${g}</mi>`).join("") + `</mrow><annotation encoding="application/x-tex">${glyphs}` +
      '</annotation></semantics></math></span><span class="katex-html" aria-hidden="true"><span class="base">' +
      '<span class="strut"></span>' + [...glyphs].map((g) => `<span class="mord">${g}</span>`).join("") +
      "</span></span></span>";
    const EXCLUDE = ".katex-mathml, .mdp-diagram svg";
    /** The page's text nodes, less KaTeX's. */
    const own = () => {
      const walker = document.createTreeWalker(document.getElementById("root")!, NodeFilter.SHOW_TEXT);
      const out: Text[] = [];
      for (let n = walker.nextNode(); n; n = walker.nextNode()) {
        if (!n.parentElement!.closest(".katex")) out.push(n as Text);
      }
      return out;
    };
    const page = async (html: string, exclude: string | null = EXCLUDE, visual?: VisualOptions) => {
      document.getElementById("root")!.innerHTML = html;
      const root = () => document.getElementById("root");
      await start(visual, undefined, { find: { root, exclude: exclude ?? undefined } });
    };
    /** Removes the page's install, before the next `page` in the same test. */
    const reset = () => {
      cleanup?.();
      cleanup = undefined;
    };
    const MODES = ["off", "text", "element"] as const;

    it("motion_katex_words: w, b and e pass a formula as they pass an image, in every boundary report", async () => {
      for (const mode of MODES) {
        upstream = mode;
        // A word ends at a block's edge in the engines (their selection text gains a line break there); the stand-in
        // has no character there, so the first paragraph ends in punctuation.
        await page(`<p>see ${katex("x+y")} now.</p><p>the area ${katex("πr2")}, then more</p>`);
        const [see, now, area, then] = own();
        expect(motion({ node: see, offset: 0 }, "w"), mode).toEqual({ node: now, offset: 1 });
        expect(motion({ node: now, offset: 1 }, "b"), mode).toEqual({ node: see, offset: 0 });
        expect(motion({ node: see, offset: 0 }, "e"), mode).toEqual({ node: see, offset: 2 });
        expect(motion({ node: see, offset: 2 }, "e"), mode).toEqual({ node: now, offset: 3 });
        expect(motion({ node: area, offset: 4 }, "w"), mode).toEqual({ node: then, offset: 2 });
        expect(motion({ node: then, offset: 2 }, "b"), mode).toEqual({ node: area, offset: 4 });
        reset();
      }
    });

    it("motion_katex_out: h, l, 0 and $ next to a formula move out of it instead of answering null", async () => {
      // One paragraph a page: the stand-in reports the start of a paragraph upstream as the end of the one before,
      // which no engine does across blocks.
      for (const mode of MODES) {
        upstream = mode;
        await page(`<p>see ${katex("x+y")} now</p>`);
        const [see, now] = own();
        expect(motion({ node: now, offset: 0 }, "h"), mode).toEqual({ node: see, offset: 3 });
        expect(motion({ node: see, offset: 3 }, "l"), mode).toEqual({ node: now, offset: 0 });
        expect(motion({ node: now, offset: 2 }, "0"), mode).toEqual({ node: see, offset: 0 });
        expect(motion({ node: see, offset: 0 }, "$"), mode).toEqual({ node: now, offset: 3 });
        reset();
        // A line that ends in math: `$` is its last character; one that starts with it: `0` is its first, and `h`
        // there moves nothing, the page's text starting with the formula.
        await page(`<p>a${katex("x")}</p>`);
        const [a] = own();
        expect(motion({ node: a, offset: 0 }, "$"), mode).toEqual({ node: a, offset: 0 });
        reset();
        await page(`<p>${katex("E=mc2")} b</p>`);
        const [b] = own();
        expect(motion({ node: b, offset: 1 }, "0"), mode).toEqual({ node: b, offset: 0 });
        expect(motion({ node: b, offset: 1 }, "h"), mode).toEqual({ node: b, offset: 0 });
        expect(motion({ node: b, offset: 0 }, "h"), mode).toEqual({ node: b, offset: 0 });
        reset();
      }
      // With a line before, the step back from a line that starts with math goes past it onto that line, as past an
      // image.
      upstream = "off";
      await page(`<p>a${katex("x")}</p><p>${katex("E")} b</p>`);
      const [a, b] = own();
      expect(motion({ node: b, offset: 0 }, "h")).toEqual({ node: a, offset: 0 });
    });

    it("never answers a place in the formula, from any character of the page's", async () => {
      for (const mode of MODES) {
        upstream = mode;
        await page(`<p>see ${katex("x+y")} now</p><p>a${katex("x")}</p><p>${katex("E=mc2")} b</p>`);
        for (const t of own()) {
          for (let offset = 0; offset < t.data.length; offset++) {
            for (const k of ["h", "l", "w", "b", "e", "0", "$"]) {
              const got = motion({ node: t, offset }, k);
              expect(got, `${mode} ${k} from ${JSON.stringify(t.data)},${offset}`).not.toBeNull();
              expect(got!.node.parentElement!.closest(".katex"), `${mode} ${k} from ${t.data},${offset}`).toBeNull();
            }
          }
        }
        reset();
      }
    });

    it("motion_hidden_word_end: e over a word running into aria-hidden text ends on the word's last character", () => {
      document.getElementById("root")!.innerHTML = '<p>ab<span aria-hidden="true">Z</span></p><p>next</p>';
      const ab = document.querySelector("p")!.firstChild as Text;
      expect(motion({ node: ab, offset: 0 }, "e")).toEqual({ node: ab, offset: 1 });
    });

    it("motion_solid_character: reads the character a step passed, not the line break the engine writes beside it",
      async () => {
        await page(`<p>so a${katex("x")}</p><p>${katex("E")} b</p>`);
        const [a, b] = own();
        // Blink's selection text gains "a\n" for the step over the `a` before a formula that ends its paragraph.
        const plain = Selection.prototype.toString;
        vi.spyOn(Selection.prototype, "toString").mockImplementation(function (this: Selection) {
          const text = plain.call(this);
          return this.rangeCount && this.getRangeAt(0).startContainer === a && this.getRangeAt(0).startOffset === 3
            ? text.replace(/^a/, "a\n") : text;
        });
        expect(motion({ node: b, offset: 1 }, "b")).toEqual({ node: a, offset: 3 });
      });

    it("motion_break_in_left_out: a line break inside left-out text is passed with it, as KaTeX's MathML lays out",
      async () => {
        breaks = true;
        // The engine's selection text has a line break where a `<br>` lies.
        vi.spyOn(Selection.prototype, "toString").mockImplementation(function (this: Selection) {
          if (this.rangeCount === 0) return "";
          const range = this.getRangeAt(0);
          return range.toString() + (range.cloneContents().querySelector("br") ? "\n" : "");
        });
        await page('<p>see <span class="clip">x<br>y</span> now</p>', ".clip");
        const [see, now] = own().filter((t) => !t.parentElement!.closest(".clip"));
        expect(motion({ node: see, offset: 3 }, "l")).toEqual({ node: now, offset: 0 });
        expect(motion({ node: now, offset: 0 }, "h")).toEqual({ node: see, offset: 3 });
      });

    it("motion_line_end_in_formula: h from a line that starts with math goes on back past a line before that ends " +
      "with it", async () => {
      // Blink's step back from the second line stops at the end of the first line's formula, and the step on from
      // there passes the white space between the paragraphs: no character of that line.
      upstream = "text";
      await page('<p>a<span class="clip">x</span></p>\n<p><span class="clip">E</span> b</p>', ".clip");
      const [a, , b] = own().filter((t) => !t.parentElement!.closest(".clip"));
      expect(motion({ node: b, offset: 0 }, "h")).toEqual({ node: a, offset: 0 });
    });

    it("motion_left_out_block: left-out text laid out as a block is on its page ancestor's line", async () => {
      // KaTeX's MathML copy is absolutely positioned (a block), and its superscripts sit in table cells.
      await page('<p>see <span class="clip" style="display: block">x</span> now</p>', ".clip");
      const [see, now] = own().filter((t) => !t.parentElement!.closest(".clip"));
      expect(motion({ node: see, offset: 3 }, "l")).toEqual({ node: now, offset: 0 });
    });

    it("motion_upstream_into_formula: a place the engine reports inside a formula for the character after it is that " +
      "character's", async () => {
      // WebKit gives the place before the space after a formula as one at the end of the formula's glyphs.
      upstream = "text";
      await page(`<p>so</p><p>${katex("E")} b</p>`);
      const [, b] = own();
      expect(motion({ node: b, offset: 1 }, "h")).toEqual({ node: b, offset: 0 });
    });

    it("motion_find_exclude: leaves out what find.exclude names, nothing for a selector the engine cannot parse",
      async () => {
        const html = '<p>see <span class="clip">zz</span> now</p>';
        const zz = () => document.querySelector(".clip")!.firstChild;
        for (const [exclude, landsIn] of [[".clip", false], [null, true], ["[[[", true]] as const) {
          await page(html, exclude);
          const [see, now] = own().filter((t) => t !== zz());
          const got = motion({ node: see, offset: 0 }, "w");
          expect(got, String(exclude)).toEqual(landsIn ? { node: zz(), offset: 0 } : { node: now, offset: 1 });
          reset();
        }
      });

    it("visual_katex: visual mode's cursor moves past a formula, never stalls before it or stands in it", async () => {
      let from: Spot = { node: document.body, offset: 0 };
      const visual = {
        start: () => {
          const r = document.createRange();
          r.setStart(from.node, from.offset);
          return r;
        },
      };
      for (const mode of MODES) {
        upstream = mode;
        await page(`<p>see ${katex("x+y")} now</p>`, EXCLUDE, visual);
        const [see, now] = own();
        const run = (at: Spot, keys: string[]) => {
          from = at;
          key("v");
          for (const k of keys) key(k);
          const out = [sel().toString(), visualCursor()];
          key("Escape");
          return out;
        };
        expect(run({ node: now, offset: 0 }, ["h"])[1], mode).toEqual({ node: see, offset: 3 });
        expect(run({ node: see, offset: 0 }, ["w"])[1], mode).toEqual({ node: now, offset: 1 });
        expect(run({ node: now, offset: 1 }, ["b"])[1], mode).toEqual({ node: see, offset: 0 });
        reset();
        await page(`<p>a${katex("x")}</p>`, EXCLUDE, visual);
        const [a] = own();
        expect(run({ node: a, offset: 0 }, ["$"]), mode).toEqual(["a", { node: a, offset: 0 }]);
        reset();
      }
    });

    it("G and gg stop at the page's last and first text, never in left-out text", async () => {
      await page(`<p>${katex("x")} first</p><p>last ${katex("y")}</p>`, EXCLUDE,
        { start: () => { const r = document.createRange(); r.setStart(own()[0], 1); return r; } });
      const [first, last] = own();
      key("v");
      key("G");
      expect(visualCursor()).toEqual({ node: last, offset: last.data.length - 1 });
      key("g");
      key("g");
      expect(sel().focusNode).toBe(first);
      key("Escape");
    });

    /** A diagram's worth of left-out text: `n` labels in a block the page excludes. */
    const diagram = (n: number) => '<div class="dg">' +
      Array.from({ length: n }, (_, i) => `<span>Node label ${i}</span>`).join("") + "</div>";
    /** Counts the engine's `modify` calls and the characters `Selection.toString` reads while `run` runs. */
    const cost = (run: () => void) => {
      const modify = (Selection.prototype as unknown as { modify: typeof fakeModify }).modify;
      const read = Selection.prototype.toString;
      let calls = 0;
      let chars = 0;
      (Selection.prototype as unknown as { modify: typeof fakeModify }).modify = function (this: Selection, ...a) {
        calls++;
        return modify.apply(this, a);
      };
      const spy = vi.spyOn(Selection.prototype, "toString").mockImplementation(function (this: Selection) {
        const text = read.call(this);
        chars += text.length;
        return text;
      });
      try {
        run();
      } finally {
        (Selection.prototype as unknown as { modify: typeof fakeModify }).modify = modify;
        spy.mockRestore();
      }
      return { calls, chars };
    };

    it("motion_left_out_one_jump: a large left-out block is crossed in one move, at a cost that does not grow with it",
      async () => {
        await page(`<p>alpha beta</p>${diagram(200)}<p>gamma delta</p>`, ".dg");
        const [alpha, gamma] = own().filter((t) => !t.parentElement!.closest(".dg"));
        const answers: (Spot | null)[] = [];
        const starts = [[{ node: alpha, offset: 6 }, "w"], [{ node: gamma, offset: 0 }, "b"],
          [{ node: gamma, offset: 0 }, "h"], [{ node: alpha, offset: 9 }, "l"], [{ node: alpha, offset: 6 }, "e"]] as const;
        for (const [from, k] of starts) {
          const spent = cost(() => answers.push(motion(from, k)));
          // `w` took 5390 calls and read 7.3 million characters when the walk stepped through the labels.
          expect(spent.calls, k).toBeLessThanOrEqual(24);
          expect(spent.chars, k).toBeLessThanOrEqual(40);
        }
        // `l` from the last character before the block stays on it: no character follows on its line.
        expect(answers).toEqual([{ node: gamma, offset: 0 }, { node: alpha, offset: 6 }, { node: alpha, offset: 9 },
          { node: alpha, offset: 9 }, { node: alpha, offset: 9 }]);
      });

    it("visual_line_past_left_out: j and k in visual mode cross a left-out block to the text beyond it", async () => {
      let from: Spot = { node: document.body, offset: 0 };
      const visual = {
        start: () => {
          const r = document.createRange();
          r.setStart(from.node, from.offset);
          return r;
        },
      };
      for (const block of [diagram(3), '<div aria-hidden="true">x<br>y</div>']) {
        await page(`<p>alpha beta</p>${block}<p>gamma delta</p><p>last</p>`, ".dg", visual);
        const [alpha, gamma, last] = own().filter((t) => !t.parentElement!.closest(".dg, [aria-hidden]"));
        from = { node: alpha, offset: 2 };
        key("v");
        key("j");
        expect(visualCursor(), block).toEqual({ node: gamma, offset: 0 });
        key("j");
        expect(visualCursor(), block).toEqual({ node: last, offset: 0 });
        key("k");
        key("k");
        expect(visualCursor(), block).toEqual({ node: alpha, offset: 9 });
        key("Escape");
        // Linewise, the selection grows past the block a line at a time.
        from = { node: alpha, offset: 2 };
        key("V");
        key("j");
        expect(sel().toString(), block).toContain("gamma delta");
        key("j");
        expect(sel().toString(), block).toContain("last");
        key("Escape");
        reset();
      }
    });

    it("motion_stuck_in_left_out: where the engine cannot step back inside left-out text, the walk goes past it",
      async () => {
        stuck = ".katex-html";
        upstream = "text";
        await page(`<p>see ${katex("x+y")} now</p>`);
        const [see, now] = own();
        expect(motion({ node: now, offset: 1 }, "b")).toEqual({ node: see, offset: 0 });
        expect(motion({ node: now, offset: 0 }, "h")).toEqual({ node: see, offset: 3 });
        expect(motion({ node: now, offset: 1 }, "h")).toEqual({ node: now, offset: 0 });
        expect(motion({ node: see, offset: 3 }, "l")).toEqual({ node: now, offset: 0 });
      });

    /** Visual mode started at `from.at`, as the page's cursor. */
    const startAt = () => {
      const from: { at: Spot } = { at: { node: document.body, offset: 0 } };
      const visual = {
        start: () => {
          const r = document.createRange();
          r.setStart(from.at.node, from.at.offset);
          return r;
        },
      };
      return { from, visual };
    };

    it("visual_inline_formula_line: j onto a line that ends with an inline formula, and k onto one that starts with " +
      "one, stay on that line", async () => {
      const { from, visual } = startAt();
      await page(`<p>aaaaaaaa</p><p>bb ${katex("xyz")}</p><p>cccccccc</p><p>${katex("xyz")} dd</p><p>eeeeeeee</p>`,
        EXCLUDE, visual);
      const [a, bb, , dd, e] = own();
      from.at = { node: a, offset: 4 };
      key("v");
      key("j");
      expect(visualCursor()).toEqual({ node: bb, offset: 2 });
      key("Escape");
      from.at = { node: e, offset: 1 };
      key("v");
      key("k");
      expect(visualCursor()).toEqual({ node: dd, offset: 0 });
      key("Escape");
    });

    it("visual_inline_formula_line_block_copy: the MathML copy laid out as an absolutely positioned block is on its " +
      "formula's line", async () => {
      const { from, visual } = startAt();
      const block = (glyphs: string) =>
        katex(glyphs).replace('class="katex-mathml"', 'class="katex-mathml" style="position: absolute; display: block"');
      await page(`<p>aaaaaaaa</p><p>bb ${block("xyz")}</p><p>cccccccc</p>`, EXCLUDE, visual);
      const [a, bb] = own();
      from.at = { node: a, offset: 4 };
      key("v");
      key("j");
      expect(visualCursor()).toEqual({ node: bb, offset: 2 });
      key("Escape");
    });

    it("visual_line_onward: a line move that lands in its own line's formula goes on to the next line", async () => {
      const { from, visual } = startAt();
      await page(`<p>bb ${katex("xyz")}</p><p>cccccccc</p>`, EXCLUDE, visual);
      const [bb, c] = own();
      // Blink's line move from the text before a formula that ends its line lands in the formula's MathML copy.
      const copy = document.querySelector(".katex-mathml mi")!.firstChild!;
      (Selection.prototype as unknown as { modify: typeof fakeModify }).modify = function (this: Selection,
        alter: string, direction: string, granularity: string) {
        if (granularity === "line" && direction === "forward" && this.focusNode === bb) this.extend(copy, 0);
        else fakeModify.call(this, alter, direction, granularity);
      };
      from.at = { node: bb, offset: 2 };
      key("v");
      key("j");
      expect(visualCursor()).toEqual({ node: c, offset: 0 });
      key("Escape");
    });

    it("left_out_first: a page that opens with a diagram: b and h from its first word never reach into it", async () => {
      const { from, visual } = startAt();
      await page('<div class="mdp-diagram"><svg><text>Node</text></svg></div><p>start words</p>', EXCLUDE, visual);
      const start = document.querySelector("p")!.firstChild as Text;
      expect(motion({ node: start, offset: 0 }, "b")).toEqual({ node: start, offset: 0 });
      expect(motion({ node: start, offset: 0 }, "h")).toBeNull();
      from.at = { node: start, offset: 0 };
      key("v");
      for (const k of ["b", "h"]) {
        key(k);
        expect(visualCursor(), k).toEqual({ node: start, offset: 0 });
        expect(sel().toString(), k).toBe("s");
      }
      key("Escape");
    });

    it("visual_end_before_left_out: the selection drawn from the last character before a diagram ends in that text",
      async () => {
        const { from, visual } = startAt();
        await page(`<p>alpha beta</p>${diagram(3)}<p>gamma</p>`, ".dg", visual);
        const [alpha] = own();
        from.at = { node: alpha, offset: 9 };
        key("v");
        key("l");
        expect(sel().focusNode).toBe(alpha);
        expect(sel().toString()).toBe("a");
        key("Escape");
      });
  });

  it("motion_cjk_run_is_one_word: b and e over a Chinese run take it whole, as w does", () => {
    cjkSplit = true;
    document.getElementById("root")!.innerHTML = "<p>中文中文 x</p>";
    expect(motion(spot("p", 0, 5), "b")).toEqual(spot("p", 0, 0));
    expect(motion(spot("p", 0, 4), "b")).toEqual(spot("p", 0, 0));
    expect(motion(spot("p", 0, 2), "b")).toEqual(spot("p", 0, 0));
    expect(motion(spot("p", 0, 0), "e")).toEqual(spot("p", 0, 3));
  });

  it("visual mode's b takes a Chinese run whole too", async () => {
    cjkSplit = true;
    document.getElementById("root")!.innerHTML = "<p>中文中文 x</p>";
    await start({ start: () => at("p", 0, 5) });
    key("v");
    key("b");
    expect(sel().toString()).toBe("中文中文 x");
  });

  it("motion_outside_root_is_null: a start outside the page, and a result that would leave it", async () => {
    document.body.insertAdjacentHTML("beforeend", "<p id=after>after</p><p aria-hidden=true id=hid>hidden</p>");
    await start();
    const out = document.getElementById("after")!.firstChild!;
    expect(motion({ node: out, offset: 1 }, "l")).toBeNull();
    expect(motion({ node: document.getElementById("hid")!.firstChild!, offset: 1 }, "l")).toBeNull();
    // From the root's last text, past its last character the next place is in the next paragraph, off the page.
    const last = document.querySelectorAll("#root p")[2].firstChild as Text;
    expect(motion({ node: last, offset: last.data.length }, "l")).toBeNull();
    // Inside the root the same walk answers.
    expect(motion({ node: last, offset: 2 }, "l")).toEqual({ node: last, offset: 3 });
  });

  it("answers null for a place the text no longer has, and for a key that is not a Normal-mode motion", () => {
    const p = spot("p", 0, 4);
    expect(motion({ node: p.node, offset: 99 }, "l")).toBeNull();
    for (const k of ["j", "k", "gg", "G", "n", "N", "{", "}", "x", ""]) expect(motion(p, k), k).toBeNull();
  });

  it("answers null where the engine has no Selection.modify", () => {
    delete (Selection.prototype as unknown as { modify?: unknown }).modify;
    expect(motion(spot("p", 0, 4), "l")).toBeNull();
  });

  it("motion_cjk_run_is_one_word: w over a Chinese run lands on the next word", () => {
    document.getElementById("root")!.innerHTML = "<p>中文中文 x</p>";
    expect(motion(spot("p", 0, 0), "w")).toEqual(spot("p", 0, 5));
  });

  it("does not touch a running visual mode's selection or cursor", async () => {
    await start({ start: () => at("p", 0, 4) });
    key("v");
    key("e");
    const before = [sel().toString(), ...ends()];
    expect(motion(spot("p", 1, 0), "l")).toEqual(spot("p", 1, 1));
    expect([sel().toString(), ...ends()]).toEqual(before);
    key("h");
    expect(sel().toString()).toBe("tw");
  });

  it("motion_block_boundaries: w over an <hr> or out of a table reaches the next text's first character, b goes " +
    "back past them, h after them reaches the end of the text before", () => {
    walls = "hr, table";
    // The stand-in has no character between two blocks, so the paragraph before the table ends in punctuation.
    document.getElementById("root")!.innerHTML = "<p>before rule</p><hr><p>after rule.</p>" +
      "<table><tbody><tr><td>cell</td></tr></tbody></table><h2>heading</h2>";
    const text = (selector: string) => document.querySelector(selector)!.firstChild as Text;
    const [before, after] = [...document.querySelectorAll("p")].map((p) => p.firstChild as Text);
    const [cell, heading] = [text("td"), text("h2")];
    expect(motion({ node: before, offset: 7 }, "w")).toEqual({ node: after, offset: 0 });
    expect(motion({ node: cell, offset: 0 }, "w")).toEqual({ node: heading, offset: 0 });
    expect(motion({ node: after, offset: 0 }, "b")).toEqual({ node: before, offset: 7 });
    expect(motion({ node: heading, offset: 0 }, "b")).toEqual({ node: cell, offset: 0 });
    // `h` from a line's first character answers the end of the line before, as after a paragraph.
    expect(motion({ node: after, offset: 0 }, "h")).toEqual({ node: before, offset: 11 });
    expect(motion({ node: heading, offset: 0 }, "h")).toEqual({ node: cell, offset: 4 });
    // With no text before the rule, `h` stays on the first character.
    document.getElementById("root")!.innerHTML = "<hr><p>first</p>";
    const first = text("p");
    expect(motion({ node: first, offset: 0 }, "h")).toEqual({ node: first, offset: 0 });
  });

  it("visual_h_after_rule: visual h from the first character after an <hr> reaches the end of the text before, as " +
    "motion's does", async () => {
    walls = "hr";
    document.getElementById("root")!.innerHTML = "<p>before rule</p><hr><p>after rule</p>";
    const [before] = [...document.querySelectorAll("p")].map((p) => p.firstChild as Text);
    await start({ start: () => at("p", 1, 0) });
    key("v");
    key("h");
    expect(visualCursor()).toEqual({ node: before, offset: 11 });
    key("Escape");
  });

  it("motion_word_past_element_place: w that stops at the element place after an image ending a paragraph stands " +
    "before the next paragraph's first character", () => {
    document.getElementById("root")!.innerHTML = "<p>tail <img></p><p>next</p>";
    const [tail, next] = [...document.querySelectorAll("p")].map((p) => p.firstChild as Text);
    expect(motion({ node: tail, offset: 0 }, "w")).toEqual({ node: next, offset: 0 });
  });
});
