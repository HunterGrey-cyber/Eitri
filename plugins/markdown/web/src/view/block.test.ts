import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { Preview } from "./preview";

/* The fake layout with characters: each block's text runs from x 0 in rows of 20 px, a character 0.5 of its font size
   wide (0.375 for i, l, r...; 1.25 for CJK and emoji; 0.25 for a space), so at the page's 16 px an `x` is 8 px and an
   `i` 6. The kit is a stub with `motion`, as kit 6 has it. */
const here = dirname(fileURLToPath(import.meta.url));
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 200, top: 1,
    top_fraction: 0, bottom: 40, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}

let layout: FakeLayout;
let p: Preview;
let root: HTMLElement;
let tick = 1;
const block = () => p.block.el;
const glyph = () => block().querySelector<HTMLElement>(".mdp-cursor-glyph")!;
const px = (v: string) => parseFloat(v);
const round = (v: string) => Math.round(px(v) * 1000) / 1000;
const box = () => ({ left: round(block().style.left), top: round(block().style.top), width: round(block().style.width),
  height: round(block().style.height) });
const band = () => document.querySelector<HTMLElement>(".mdp-cursor-line")!;
const kitWithMotion = () => { (window as { eitriKeys?: unknown }).eitriKeys = { version: 6, motion: () => null }; };

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.head.innerHTML = "";
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root, { chars: true });
  root.style.lineHeight = "20px";
  root.style.fontSize = "16px";
  kitWithMotion();
  const bridge: Bridge = { call: async () => ({ applied: true }), on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
  tick = 1;
});
afterEach(() => {
  p.dispose();
  layout.restore();
  delete (window as { eitriKeys?: unknown }).eitriKeys;
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function show(body: string, v: Partial<EditorView> = {}): void {
  p.onView(view(v));
  p.onText({ buf: 1, tick: tick++, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}
/** A collapsed range in the first text node holding `text`, at `at` code units into it. */
function at(text: string, offset = 0): Range {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode() as Text | null; n; n = walker.nextNode() as Text | null) {
    const i = n.data.indexOf(text);
    if (i >= 0) { const r = document.createRange(); r.setStart(n, i + offset); return r; }
  }
  throw new Error(`no text ${text}`);
}
function place(text: string, offset = 0): void { p.block.placeAt(at(text, offset)); p.drawBlock(); }
function sheet(css: string): void { const s = document.createElement("style"); s.textContent = css; document.head.append(s); }

describe("the block cursor", () => {
  it("draws on the first character when the keys arrive", () => {
    show("  hello world\n\nnext");
    expect(block().hidden).toBe(true);
    p.onFocus(true);
    expect(block().hidden).toBe(false);
    expect(glyph().textContent).toBe("h");
    // `h` is 8 px from x 0, so the block is 9.6 px wide, 0.8 px either side of it, on the band's row.
    expect(box()).toEqual({ left: -0.8, top: 0, width: 9.6, height: 20 });
    expect(p.block.index()).toBe(0);
    expect(p.block.caretRange()!.startContainer.textContent).toContain("hello");
  });

  it("is as wide as the glyph or 0.6 em, centred on it", () => {
    show("hi 中 `il` x\n\n| hi | x |\n|----|---|\n| a  | b |");
    sheet("code { font-size: 13px; } th, td { font-size: 10px; }");
    p.onFocus(true);
    // `i` is 6 px at 16 px: 9.6 px, starting 1.8 px left of the glyph (`h` before it is 8 px).
    place("hi", 1);
    expect(box()).toMatchObject({ left: 8 - 1.8, width: 9.6 });
    // A CJK glyph is wider than 0.6 em and keeps its width: `hi ` is 8 + 6 + 4 px.
    place("中");
    expect(box()).toMatchObject({ left: 18, width: 20 });
    // Inline code at 13 px: its `i` (4.875 px) gets 0.6 x 13 = 7.8 px.
    place("il");
    expect(box().width).toBeCloseTo(7.8);
    expect(glyph().style.fontSize).toBe("13px");
    // A table cell's own size: 10 px, so 6 px for its `h` (5 px wide); the table scrolls sideways and the cell is at its
    // left edge, so the half pixel the block would stand out on the left is cut.
    p.moveRows(1);
    const th = document.createRange();
    th.setStart(root.querySelector("th")!.firstChild!, 0);
    p.block.placeAt(th);
    p.drawBlock();
    expect(glyph().textContent).toBe("h");
    expect(block().hidden).toBe(false);
    expect(box().width).toBeCloseTo(5.5);
  });

  it("is as tall as the character's line, within the band", () => {
    show("hello\n\nnext");
    sheet("p { line-height: 30px; }");
    p.onFocus(true);
    expect(box()).toMatchObject({ top: 0, height: 20 });
    sheet("p { line-height: 12px; }");
    p.drawBlock();
    // The 20 px glyph's middle is the row's: 6 px either side of it.
    expect(box()).toMatchObject({ top: 4, height: 12 });
  });

  it("paints above the band and lets clicks through", () => {
    sheet(readFileSync(resolve(here, "../style/chrome.css"), "utf8"));
    show("hello\n\nnext");
    p.onFocus(true);
    expect(band().compareDocumentPosition(block()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    const cs = getComputedStyle(block());
    expect(cs.position).toBe("absolute");
    expect(cs.pointerEvents).toBe("none");
    expect(cs.userSelect).toBe("none");
    expect(cs.mixBlendMode === "" || cs.mixBlendMode === "normal").toBe(true);
    expect(cs.zIndex === "" || cs.zIndex === "auto").toBe(true);
    expect(block().getAttribute("aria-hidden")).toBe("true");
    expect(cs.backgroundColor).toBe("var(--nv-cursor-bg, var(--nv-fg))");
    expect(cs.color).toBe("var(--nv-cursor-fg, var(--nv-bg))");
  });

  it("copies the glyph as text", () => {
    show("a \\<b\\> c\n\nnext");
    p.onFocus(true);
    place("<b>");
    expect(glyph().textContent).toBe("<");
    expect(glyph().children.length).toBe(0);
    expect(glyph().innerHTML).toBe("&lt;");
    const source = readFileSync(resolve(here, "block.ts"), "utf8");
    for (const word of ["innerHTML", "outerHTML", "insertAdjacentHTML", "DOMParser"]) expect(source).not.toContain(word);
  });

  it("exists only while the preview has the keys and the kit has its motions", () => {
    show("hello\n\nnext");
    expect(block().hidden).toBe(true);
    p.onFocus(true);
    expect(block().hidden).toBe(false);
    p.onFocus(false);
    expect(block().hidden).toBe(true);
    // A kit before 6: the band alone.
    (window as { eitriKeys?: unknown }).eitriKeys = { version: 5 };
    p.onFocus(true);
    expect(band().hidden).toBe(false);
    expect(block().hidden).toBe(true);
    kitWithMotion();
    p.drawBlock();
    expect(block().hidden).toBe(false);
  });

  it("stays while visual mode hides the band", () => {
    show("hello\n\nnext");
    p.onFocus(true);
    document.documentElement.setAttribute("data-eitri-visual", "char");
    try {
      p.drawBlock();
      expect(block().hidden).toBe(false);
    } finally {
      document.documentElement.removeAttribute("data-eitri-visual");
    }
  });

  it("draws nothing on a row without text", () => {
    show("![HUGE](x.png)\n\nnext\n\n---\n\nlast");
    p.onFocus(true);
    expect(band().hidden).toBe(false);
    expect(block().hidden).toBe(true);
    // The text after the image has the block; the rule after it has the band alone.
    p.jumpCursorTo(layout.boxOf(root.querySelectorAll("p")[1])!.top + 1);
    expect(glyph().textContent).toBe("n");
    expect(block().hidden).toBe(false);
    p.jumpCursorTo(layout.boxOf(root.querySelector("hr")!)!.top);
    expect(band().hidden).toBe(false);
    expect(block().hidden).toBe(true);
  });

  it("skips collapsed space and keeps surrogate pairs", () => {
    show("ab  cd \u{1F600}x\n\nnext");
    p.onFocus(true);
    // The second of two spaces draws nothing: the block goes on to `c`.
    place("ab", 3);
    expect(glyph().textContent).toBe("c");
    expect(p.block.index()).toBe(3);
    // An offset inside the emoji's pair is the emoji, whole: one block, 20 px, one index.
    place("\u{1F600}", 1);
    expect(glyph().textContent).toBe("\u{1F600}");
    expect(p.block.caretRange()!.startOffset).toBe("ab  cd ".length);
    expect(box().width).toBe(20);
    place("x");
    expect(p.block.index()).toBe(7);
  });

  it("survives a morph that replaces its text node", () => {
    show("abcdef ghi\n\nnext");
    p.onFocus(true);
    place("def");
    expect(p.block.index()).toBe(3);
    const before = p.block.caretRange()!.startContainer;
    // Emphasis around the start: the text node the block was on is replaced; the block keeps its index.
    show("*ab*cdef ghi\n\nnext");
    const range = p.block.caretRange()!;
    expect(range.startContainer).not.toBe(before);
    expect(range.startContainer.isConnected).toBe(true);
    expect(glyph().textContent).toBe("d");
    expect(p.block.index()).toBe(3);
    // A shorter row: its last character.
    show("*ab*\n\nnext");
    expect(p.block.caretRange()!.startContainer.isConnected).toBe(true);
    expect(glyph().textContent).toBe("b");
    expect(p.block.index()).toBe(1);
  });

  it("goes back to its index after a morph that keeps its text node", () => {
    show("ab **cdef**\n\nnext");
    p.onFocus(true);
    place("cdef", 2);
    expect(glyph().textContent).toBe("e");
    expect(p.block.index()).toBe(5);
    const bold = root.querySelector("strong")!.firstChild;
    // Text before the emphasis grows: the emphasised text node is the same one, and the index stays where it was.
    show("abcd **cdef**\n\nnext");
    expect(root.querySelector("strong")!.firstChild).toBe(bold);
    expect(glyph().textContent).toBe("c");
    expect(p.block.index()).toBe(5);
    expect(p.block.caretRange()!.startContainer).toBe(bold);
  });

  it("follows a sideways scroll and hides outside the clip", () => {
    show(`\`\`\`\n${"x".repeat(100)}\n\`\`\`\n\nnext`);
    p.onFocus(true);
    const pre = root.querySelector<HTMLElement>("pre.mdp-wide")!;
    place("x", 50);
    expect(p.block.index()).toBe(50);
    expect(box().left).toBeCloseTo(400 - 0.8);
    const scrollTo = (x: number) => {
      Object.defineProperty(pre, "scrollLeft", { value: x, configurable: true, writable: true });
      pre.dispatchEvent(new Event("scroll"));
    };
    scrollTo(100);
    expect(block().hidden).toBe(false);
    expect(box().left).toBeCloseTo(300 - 0.8);
    // The character is scrolled out of the block's 600 px box: no block until it is back.
    scrollTo(450);
    expect(block().hidden).toBe(true);
    scrollTo(0);
    expect(block().hidden).toBe(false);
    expect(p.block.index()).toBe(50);
  });

  it("draws the band in the editor's CursorLine colour and visual mode in the selection colour", () => {
    sheet(readFileSync(resolve(here, "../style/chrome.css"), "utf8"));
    show("hello\n\nnext");
    p.onFocus(true);
    // The colour is on the band's two layers (`style.test.ts`), the band itself paints none.
    expect(band().hidden).toBe(false);
    expect(getComputedStyle(band()).backgroundColor).not.toContain("var(");
    for (const cls of ["mdp-visual-band", "mdp-visual-box"]) {
      const el = document.createElement("div");
      el.className = cls;
      document.body.append(el);
      expect(getComputedStyle(el).backgroundColor, cls).toBe("var(--nv-cursorline)");
    }
  });
});

interface Cost { reads: number; rects: number; steps: number; chars: number; checks: number }
const KINDS = ["reads", "rects", "steps", "chars", "checks"] as const;
/** What `moves` costs in the block's own code (`block.ts`, `text-from.ts`, `chars.ts`; not the preview's rows, not
 *  the fake layout): `reads` of a box (a range's or an element's), the `rects` those reads return (a range over a
 *  whole code block returns one per line, however few calls it takes), the `steps` of its tree walks, the reads
 *  of a text node's characters (`chars`: a walk over a row's characters reads the node's data at each one, even
 *  where it asks the layout nothing), and the `checks` of whether a node is still in the document (a look at every
 *  text node of a kept row costs one each). */
function costOf(moves: () => void): Cost {
  const n: Cost = { reads: 0, rects: 0, steps: 0, chars: 0, checks: 0 };
  const mine = () => {
    const stack = new Error().stack ?? "";
    return /\/view\/(block|text-from|chars)\.ts/.test(stack) && !/fakelayout\.ts/.test(stack);
  };
  let inside = 0;
  const restore: (() => void)[] = [];
  const count = (proto: object, name: string, many: boolean) => {
    const d = Object.getOwnPropertyDescriptor(proto, name)!;
    const real = d.value as (this: unknown) => unknown;
    Object.defineProperty(proto, name, {
      configurable: true, value: function (this: unknown) {
        inside++;
        try {
          const out = real.call(this);
          if (inside === 1 && mine()) { n.reads++; n.rects += many ? (out as ArrayLike<unknown>).length : 1; }
          return out;
        } finally { inside--; }
      },
    });
    restore.push(() => Object.defineProperty(proto, name, d));
  };
  count(Range.prototype, "getClientRects", true);
  count(Range.prototype, "getBoundingClientRect", false);
  count(Element.prototype, "getBoundingClientRect", false);
  const data = Object.getOwnPropertyDescriptor(CharacterData.prototype, "data")!;
  Object.defineProperty(CharacterData.prototype, "data", {
    configurable: true, get(this: CharacterData) { if (inside === 0 && mine()) n.chars++; return data.get!.call(this); },
    set(this: CharacterData, v: string) { data.set!.call(this, v); },
  });
  restore.push(() => Object.defineProperty(CharacterData.prototype, "data", data));
  const connected = Object.getOwnPropertyDescriptor(Node.prototype, "isConnected")!;
  Object.defineProperty(Node.prototype, "isConnected", {
    configurable: true, get(this: Node) {
      // The block's own looks only: the selector engine under `matches` and `getComputedStyle` asks it too.
      if (inside === 0 && /\/view\/(block|text-from|chars)\.ts/.test(new Error().stack?.split("\n")[2] ?? "")) n.checks++;
      return connected.get!.call(this);
    },
  });
  restore.push(() => Object.defineProperty(Node.prototype, "isConnected", connected));
  const realCreate = document.createTreeWalker;
  document.createTreeWalker = function (this: Document, ...args: Parameters<Document["createTreeWalker"]>) {
    const w = realCreate.apply(this, args);
    if (!mine()) return w;
    const next = w.nextNode.bind(w);
    const prev = w.previousNode.bind(w);
    w.nextNode = () => { n.steps++; return next(); };
    w.previousNode = () => { n.steps++; return prev(); };
    return w;
  } as Document["createTreeWalker"];
  restore.push(() => { document.createTreeWalker = realCreate; });
  const limit = Error.stackTraceLimit;
  Error.stackTraceLimit = 60;
  try {
    moves();
  } finally {
    for (const undo of restore.reverse()) undo();
    Error.stackTraceLimit = limit;
  }
  return n;
}
/** How many character codes the block's own code reads from strings (`charCodeAt`) during `moves`: a loop over a
 *  node's data that asks the layout nothing (a run of line breaks passed one by one) shows here, not in `costOf`. */
function codeReads(moves: () => void): number {
  const real = String.prototype.charCodeAt;
  let n = 0;
  let busy = false;
  String.prototype.charCodeAt = function (this: string, i: number) {
    if (!busy) {
      // The stack is read with the counting off: reading it decodes source maps, which reads character codes too.
      busy = true;
      try {
        if (/\/view\/(block|text-from|chars)\.ts/.test(new Error().stack?.split("\n")[2] ?? "")) n++;
      } finally { busy = false; }
    }
    return real.call(this, i);
  };
  try {
    moves();
  } finally {
    String.prototype.charCodeAt = real;
  }
  return n;
}
const worst = (costs: Cost[]): Cost =>
  Object.fromEntries(KINDS.map((k) => [k, Math.max(...costs.map((c) => c[k]))])) as unknown as Cost;
/** Expects `b` to cost no more than `a` and `slack` of each. */
function noMoreThan(b: Cost, a: Cost, slack: number): void {
  for (const k of KINDS) expect(b[k], k).toBeLessThanOrEqual(a[k] + slack);
}

/** Folds every closed `details` as the engine does: its content (all but the summary) has no box, neither its
 *  elements nor its text; the fake layout would give them one. Returns the undo. */
function fold(): () => void {
  const folded = (n: Node) => {
    const el = n instanceof Element ? n : n.parentElement;
    return !!el?.closest("details:not([open])") && !el.closest("summary") && !(el.matches("details"));
  };
  const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
  const rects = proto.getClientRects;
  const rangeBox = proto.getBoundingClientRect;
  const elBox = Element.prototype.getBoundingClientRect;
  const zero = { top: 0, bottom: 0, left: 0, right: 0, width: 0, height: 0, x: 0, y: 0, toJSON: () => ({}) } as DOMRect;
  Object.defineProperty(proto, "getClientRects", {
    configurable: true, value: function (this: Range) { return folded(this.startContainer) ? [] : rects.call(this); },
  });
  Object.defineProperty(proto, "getBoundingClientRect", {
    configurable: true, value: function (this: Range) { return folded(this.startContainer) ? zero : rangeBox.call(this); },
  });
  Element.prototype.getBoundingClientRect = function (this: Element) { return folded(this) ? zero : elBox.call(this); };
  return () => {
    Object.defineProperty(proto, "getClientRects", { configurable: true, value: rects });
    Object.defineProperty(proto, "getBoundingClientRect", { configurable: true, value: rangeBox });
    Element.prototype.getBoundingClientRect = elBox;
  };
}

describe("the cost of the block", () => {
  const fence = (lines: number) =>
    `p0\n\n\`\`\`rust\n${Array.from({ length: lines }, (_, i) => `let x${i} = ${i}; // line ${i}`).join("\n")}\n\`\`\`\n\np1`;
  /** The most one `j` costs over five of them, with the cursor `line` lines into the fence. */
  function perMove(line: number): Cost {
    p.jumpCursorTo(layout.boxOf(root.querySelector("pre")!)!.top + line * 20 + 1);
    p.moveRows(1);
    return worst(Array.from({ length: 5 }, () => costOf(() => p.moveRows(1))));
  }

  it("does not grow with the length of the code block or the cursor's depth in it", () => {
    show(fence(60));
    p.onFocus(true);
    const short = perMove(20);
    show(fence(300));
    const shallow = perMove(20);
    const deep = perMove(250);
    expect(glyph().textContent).toBe("l");
    // The highlighted block is thousands of text nodes: no read returns its lines, and no walk goes over them.
    expect(deep.reads).toBeLessThanOrEqual(80);
    expect(deep.rects).toBeLessThanOrEqual(80);
    expect(deep.steps).toBeLessThanOrEqual(10);
    noMoreThan(shallow, short, 10);
    noMoreThan(deep, short, 10);
  });

  it("does not grow with the length of the row, for j onto it, nor with the column, for l along it", () => {
    const rows = (n: number) => `\`\`\`\n${Array.from({ length: 4 }, () => "x".repeat(n)).join("\n")}\n\`\`\``;
    const cost = (n: number) => {
      show(rows(n));
      p.onFocus(true);
      p.jumpCursorTo(layout.boxOf(root.querySelector("pre")!)!.top + 1);
      place("x", n - 10);
      p.moveRows(1);
      expect(p.block.index()).toBe(n - 10);
      return costOf(() => p.moveRows(1));
    };
    const short = cost(40);
    const long = cost(2000);
    // Halving over a text node's characters, and counting the ones before the block by halving over their starts,
    // are logarithmic in its length; a read, or a look, at each character is not.
    expect(long.reads).toBeLessThanOrEqual(100);
    expect(long.chars).toBeLessThanOrEqual(300);
    noMoreThan(long, { ...short, reads: 2 * short.reads, rects: 2 * short.rects, chars: 2 * short.chars }, 0);
    // l at the start and near the end of the same long row costs the same.
    const at = (col: number) => {
      place("x", col);
      return worst(Array.from({ length: 3 }, () => costOf(() => expect(p.blockMove("l")).toBe(true))));
    };
    const early = at(5);
    noMoreThan(at(1900), early, 2);
    expect(early.chars).toBeLessThanOrEqual(150);
  });

  /** The most one `j` costs over five of them, from `line` lines into the first code block. */
  function jIn(body: string, line: number): Cost {
    show(body);
    p.onFocus(true);
    p.jumpCursorTo(layout.boxOf(root.querySelector("pre")!)!.top + line * 20 + 1);
    p.moveRows(1);
    return worst(Array.from({ length: 5 }, () => costOf(() => p.moveRows(1))));
  }

  it("does not grow with the lines of one text node: a code block with no language, a long comment token", () => {
    const plain = (lines: number) => `p0\n\n\`\`\`\n${Array.from({ length: lines }, (_, i) => `plain line ${i}`).join("\n")}\n\`\`\`\n\np1`;
    const comment = (lines: number) =>
      `p0\n\n\`\`\`rust\n/*\n${Array.from({ length: lines }, (_, i) => ` * comment line ${i}`).join("\n")}\n */\n\`\`\`\n\np1`;
    for (const body of [plain, comment]) {
      show(body(5));
      // Each block is one text node over all its lines (the comment inside its one highlight span).
      expect(at("line 4").startContainer.textContent).toContain("line 0");
      const short = jIn(body(40), 20);
      const long = jIn(body(800), 20);
      const deep = jIn(body(800), 700);
      // A range over the whole node returns one rect per line, 800 here; halving over its characters costs a half more
      // at most for twenty times the length.
      expect(deep.rects).toBeLessThanOrEqual(100);
      const half = Object.fromEntries(KINDS.map((k) => [k, 1.5 * short[k]])) as unknown as Cost;
      noMoreThan(long, half, 0);
      noMoreThan(deep, half, 0);
    }
  });

  it("does not grow with a run of blank lines in one text node, in its middle or at its end", () => {
    const top = Array.from({ length: 8 }, (_, i) => `top ${i}`).join("\n");
    const middle = (blank: number) => `p0\n\n\`\`\`\n${top}${"\n".repeat(blank + 1)}last\n\`\`\`\n\np1`;
    const end = (blank: number) => `p0\n\n\`\`\`\n${top}${"\n".repeat(blank)}\n\`\`\`\n\np1`;
    for (const gap of [middle, end]) {
      const short = jIn(gap(5), 0);
      expect(glyph().textContent).toBe("t");
      const long = jIn(gap(500), 0);
      // The row's ends are found by halving over the node, whose middle is in the blank run, and its last drawn
      // character is read back from its end: a read at each line break there is 500 a key.
      expect(long.reads).toBeLessThanOrEqual(100);
      noMoreThan(long, { ...short, reads: 1.5 * short.reads, rects: 1.5 * short.rects, chars: 1.5 * short.chars }, 0);
    }
  });

  it("passes a long run of line breaks by halving, not by a walk over it, from each row it visits", () => {
    // A few lines, then the run at the fence's end: every row's halving and its last-character search meet the run.
    const top = Array.from({ length: 4 }, (_, i) => `top ${i}`).join("\n");
    const fence = (blank: number) => `\`\`\`\n${top}${"\n".repeat(blank)}\n\`\`\``;
    const cost = (blank: number) => {
      show(fence(blank));
      p.onFocus(true);
      p.jumpCursorTo(layout.boxOf(root.querySelector("pre")!)!.top + 1);
      expect(glyph().textContent).toBe("t");
      const n = codeReads(() => p.moveRows(1));
      expect(p.cursorBand()!.top).toBe(layout.boxOf(root.querySelector("pre")!)!.top + 20);
      return n;
    };
    const short = cost(100);
    // A walk over the run from each probe that lands in it: 436 reads with 100 line breaks, 150,458 with 10,000.
    expect(cost(10000)).toBeLessThanOrEqual(short + 20);
    expect(short).toBeLessThanOrEqual(200);
  });

  it("reads only the row's characters for its direction: a Hebrew line in one text node leaves the others halved", () => {
    const fence = (width: number) =>
      `p0\n\n\`\`\`\n${Array.from({ length: 12 }, (_, i) => `${i % 10}${"x".repeat(width)}`).join("\n")}\n# \u05e9\u05dc\u05d5\u05dd\n\`\`\`\n\np1`;
    const short = jIn(fence(40), 0);
    expect(glyph().textContent).toBe("6");
    const long = jIn(fence(1500), 0);
    // The nearest-character search a right-to-left row takes reads every character's box: 1500 a row here.
    expect(long.reads).toBeLessThanOrEqual(100);
    noMoreThan(long, { ...short, reads: 2 * short.reads, rects: 2 * short.rects, chars: 2 * short.chars }, 0);
  });

  it("l along a long line of many highlighted nodes looks at none of the row's other nodes", () => {
    const minified = (n: number) => `\`\`\`rust\n${"let a=1;".repeat(n)}\n\`\`\``;
    const cost = (n: number) => {
      show(minified(n));
      p.onFocus(true);
      p.jumpCursorTo(layout.boxOf(root.querySelector("pre")!)!.top + 1);
      expect(root.querySelectorAll("pre span").length).toBeGreaterThanOrEqual(n);
      place("=", 0);
      expect(p.blockMove("l")).toBe(true);
      return worst(Array.from({ length: 5 }, () => costOf(() => expect(p.blockMove("l")).toBe(true))));
    };
    const few = cost(20);
    const many = cost(200);
    // A look at every node of the kept row is 1200 checks a key here.
    expect(many.checks).toBeLessThanOrEqual(10);
    noMoreThan(many, few, 2);
  });

  it("passes a closed details' folded text as one read", () => {
    const folded = Array.from({ length: 60 }, (_, i) => `folded line ${i} `.repeat(4)).join("\n");
    show(`before\n\n<details><summary>Sample</summary><pre>${folded}</pre></details>\n\nafter it`);
    p.onFocus(true);
    const unfold = fold();
    try {
      expect(glyph().textContent).toBe("b");
      const c = costOf(() => p.moveRows(1));
      expect(glyph().textContent).toBe("a");
      // Entering the row reads its start: the text before it is 3600 characters of folded code, passed as one node.
      expect(c.reads).toBeLessThanOrEqual(80);
    } finally {
      unfold();
    }
  });

  /** Front matter of `lines` highlighted lines (closed, as the page draws it), then a paragraph. */
  const afterFrontMatter = (lines: number) =>
    `---\n${Array.from({ length: lines }, (_, i) => `key${i}: [${i}, "v${i}"] # note ${i}`).join("\n")}\n---\n\nafter it`;

  it("h at a paragraph's start after closed front matter reads nothing before the paragraph", () => {
    show(afterFrontMatter(120));
    p.onFocus(true);
    const unfold = fold();
    try {
      p.jumpCursorTo(layout.boxOf(root.querySelector("p")!)!.top + 1);
      place("after");
      expect(glyph().textContent).toBe("a");
      const c = costOf(() => expect(p.blockMove("h")).toBe(false));
      expect(glyph().textContent).toBe("a");
      expect(c.reads).toBeLessThanOrEqual(6);
      expect(c.steps).toBeLessThanOrEqual(4);
    } finally {
      unfold();
    }
  });

  it("a place inside folded text goes to the drawn character before the fold, stepping past it whole", () => {
    show(afterFrontMatter(120));
    p.onFocus(true);
    const unfold = fold();
    try {
      p.jumpCursorTo(layout.boxOf(root.querySelector("p")!)!.top + 1);
      place("after");
      expect(glyph().textContent).toBe("a");
      // The kit's `b` answers a place inside the folded YAML: the block takes the last drawn character before it.
      const yaml = at("key60").startContainer;
      (window as { eitriKeys?: unknown }).eitriKeys = { version: 6, motion: () => ({ node: yaml, offset: 1 }) };
      const c = costOf(() => expect(p.blockMove("b")).toBe(true));
      expect(glyph().textContent).toBe("r");
      expect(p.block.caretRange()!.startContainer.textContent).toBe("Front matter");
      expect(c.steps).toBeLessThanOrEqual(10);
      expect(c.reads).toBeLessThanOrEqual(30);
    } finally {
      unfold();
    }
  });
});

describe("the block's box", () => {
  it("keeps the glyph on the text's baseline", () => {
    show("hello\n\nnext");
    p.onFocus(true);
    // The glyph's box is the character's (20 px at 16 px), and its line box as tall as that box.
    expect(glyph().style.height).toBe("20px");
    expect(glyph().style.lineHeight).toBe("20px");
    expect(px(glyph().style.top)).toBe(0);
    sheet("p { line-height: 12px; }");
    p.drawBlock();
    // The block shrinks to the 12 px line, 4 px down; the glyph keeps its place on the page, 4 px above the block's top.
    expect(box().top).toBe(4);
    expect(glyph().style.lineHeight).toBe("20px");
    expect(px(glyph().style.top)).toBe(-4);
  });

  it("takes the character's own height when its line box has none", () => {
    show("hello\n\nnext");
    sheet("p { font-size: 8px; line-height: 0; }");
    p.onFocus(true);
    expect(box().height).toBe(10);
    expect(block().hidden).toBe(false);
  });

  it("is cut to the box of the block it scrolls in", () => {
    show(`\`\`\`\n${"i".repeat(4)}${"x".repeat(80)}\n\`\`\`\n\nnext`);
    p.onFocus(true);
    // The `i` at the left edge is 6 px wide and its block 9.6: 1.8 px would stand out on the left, and are cut.
    place("ii");
    expect(glyph().textContent).toBe("i");
    expect(box()).toMatchObject({ left: 0, width: 7.8 });
    expect(px(glyph().style.left)).toBeCloseTo(0);
    // The `x` that ends at the right edge (592 to 600 px) has 0.8 px more on the right than the box has.
    place("x", 71);
    expect(glyph().textContent).toBe("x");
    expect(box().left).toBeCloseTo(592 - 0.8);
    expect(box().width).toBeCloseTo(8.8);
  });

  it("is cut to the scrollport, inside the block's border and left of its scroll bar", () => {
    show(`\`\`\`\n${"i".repeat(4)}${"x".repeat(80)}\n\`\`\`\n\nnext`);
    p.onFocus(true);
    // A 3 px border on the left and 587 px of scrollport after it (a scroll bar takes the rest): the scrollport runs
    // from x 3 to 590. Scrolled 2 px, the second `i` is at 4 to 10 and its block, 9.6 wide, would start at 2.2.
    const pre = root.querySelector<HTMLElement>("pre.mdp-wide")!;
    Object.defineProperty(pre, "clientLeft", { value: 3, configurable: true });
    Object.defineProperty(pre, "clientWidth", { value: 587, configurable: true });
    Object.defineProperty(pre, "scrollLeft", { value: 2, configurable: true, writable: true });
    place("ii", 1);
    expect(box()).toMatchObject({ left: 3, width: 8.8 });
    // The `x` from 582 to 590 ends at the scrollport's edge: its block's 0.8 px on the right are cut.
    place("x", 70);
    expect(box().left).toBeCloseTo(582 - 0.8);
    expect(box().width).toBeCloseTo(8.8);
  });
});

describe("a character is a grapheme cluster", () => {
  it("is one block, with its marks, and one step", () => {
    show("aéx\u{1F468}‍\u{1F469}‍\u{1F467}y\n\nnext");
    p.onFocus(true);
    place("e");
    // `e` and its acute accent: one character, 8 px, whose block shows both.
    expect(glyph().textContent).toBe("é");
    expect(box().width).toBeCloseTo(9.6);
    expect(p.block.index()).toBe(1);
    p.blockMove("l");
    expect(glyph().textContent).toBe("x");
    p.blockMove("l");
    // The family is one emoji, 20 px wide.
    expect(glyph().textContent).toBe("\u{1F468}‍\u{1F469}‍\u{1F467}");
    expect(box().width).toBe(20);
    expect(p.block.index()).toBe(3);
    p.blockMove("l");
    expect(glyph().textContent).toBe("y");
    p.blockMove("h");
    p.blockMove("h");
    p.blockMove("h");
    expect(glyph().textContent).toBe("é");
    // A place inside the cluster is the cluster.
    place("e", 1);
    expect(glyph().textContent).toBe("é");
    expect(p.block.caretRange()!.startOffset).toBe(1);
  });
});

describe("a block that scrolls sideways follows the block, as Neovim's 'sidescroll' follows the cursor", () => {
  const wide = (lines: string[]) => show(`\`\`\`\n${lines.join("\n")}\n\`\`\`\n\nnext`);
  const pre = () => root.querySelector<HTMLElement>("pre.mdp-wide")!;

  it("scrolls by the least that shows the character at its 0.6 em width, for $ and 0, l and w", () => {
    wide(["x".repeat(100)]);
    p.onFocus(true);
    expect(pre().scrollLeft).toBe(0);
    expect(p.blockMove("$")).toBe(true);
    // The last `x` is at 792 to 800 px and its block 0.8 px wider each side: 200.8 px brings its right edge to 600.
    expect(pre().scrollLeft).toBeCloseTo(200.8);
    expect(block().hidden).toBe(false);
    expect(glyph().textContent).toBe("x");
    expect(box().left + box().width).toBeCloseTo(600);
    expect(p.blockMove("0")).toBe(true);
    expect(block().hidden).toBe(false);
    expect(box().left).toBeCloseTo(0);
    // l along a line inside the scrollport scrolls nothing; past its right edge, a character's width at a time.
    for (let i = 0; i < 73; i++) p.blockMove("l");
    expect(pre().scrollLeft).toBeCloseTo(-0.8);
    p.blockMove("l");
    expect(block().hidden).toBe(false);
    expect(pre().scrollLeft).toBeCloseTo(0.8);
  });

  it("keeps the column for j and k with the text it scrolled, not with the window", () => {
    wide(["x".repeat(100), "y".repeat(100), "z".repeat(100)]);
    p.onFocus(true);
    // A find's match past the scrollport: the block is there, hidden, until a move.
    place("x", 90);
    expect(block().hidden).toBe(true);
    p.moveRows(1);
    expect(glyph().textContent).toBe("y");
    expect(p.block.index()).toBe(90);
    expect(block().hidden).toBe(false);
    p.moveRows(1);
    expect(p.block.index()).toBe(90);
    expect(block().hidden).toBe(false);
  });

  it("is not followed for zh and zl, and v then starts at the block's character, scrolled out or not", () => {
    wide(["x".repeat(100)]);
    p.onFocus(true);
    p.blockMove("$");
    const scrollTo = (x: number) => { pre().scrollLeft = x; pre().dispatchEvent(new Event("scroll")); };
    scrollTo(0);
    expect(block().hidden).toBe(true);
    expect(p.block.caretRange()!.startOffset).toBe(99);
    expect(p.visualStart()!.startOffset).toBe(99);
    expect(pre().scrollLeft).toBe(0);
  });
});

describe("a code line of spaces alone", () => {
  it("has the block on a space: j onto it, a click on its second space, v from it", () => {
    show("```\nfoo\n   \nbar\n```");
    p.onFocus(true);
    p.moveRows(1);
    expect(block().hidden).toBe(false);
    expect(glyph().textContent).toBe(" ");
    expect(p.block.index()).toBe(0);
    expect(p.visualStart()!.startOffset).toBe(4);
    const top = layout.boxOf(root.querySelector("pre")!)!.top;
    p.cursorToPoint(5, top + 30);
    expect(p.block.index()).toBe(1);
    expect(p.cursorBand()!.top).toBe(top + 20);
    // A line with nothing at all still has the band alone.
    show("```\nfoo\n\nbar\n```");
    p.jumpCursorTo(top + 21);
    expect(block().hidden).toBe(true);
  });
});

describe("a letter of a joining script", () => {
  it("is copied with a zero-width joiner on each side it connects to its neighbour", () => {
    const zwj = "\u200d";
    show("\u0628\u062a\u0628 \u0627\u0628 \u0628\u064e\u062a x\n\nnext");
    p.onFocus(true);
    const copy = (text: string, offset: number) => { place(text, offset); return glyph().textContent; };
    // Beh teh beh: the middle letter connects on both sides, the first only to the next, the last only back.
    expect(copy("\u0628\u062a\u0628", 1)).toBe(`${zwj}\u062a${zwj}`);
    expect(copy("\u0628\u062a\u0628", 0)).toBe(`\u0628${zwj}`);
    expect(copy("\u0628\u062a\u0628", 2)).toBe(`${zwj}\u0628`);
    // Alef connects only to the letter before it: nothing on its left, and the beh after it starts a new form.
    expect(copy("\u0627\u0628", 0)).toBe("\u0627");
    expect(copy("\u0627\u0628", 1)).toBe("\u0628");
    // A mark is passed over: beh with its fatha connects to the teh after them.
    expect(copy("\u0628\u064e\u062a", 0)).toBe(`\u0628\u064e${zwj}`);
    expect(copy("\u0628\u064e\u062a", 2)).toBe(`${zwj}\u062a`);
    expect(copy("x", 0)).toBe("x");
    expect(glyph().style.direction).toBe("ltr");
    // In the letter's direction: a left-to-right copy makes the joiners runs of their own, and WebKit then shapes
    // the letter alone, in its isolated form.
    copy("\u0628\u062a\u0628", 1);
    expect(glyph().style.direction).toBe("rtl");
  });
});

describe("the end of the row that $ keeps", () => {
  it("is the last character of a right-to-left row that j goes to", () => {
    show("ab\n\n\u05d0\u05d1\u05d2\u05d3\u05d4\n\nnext");
    p.onFocus(true);
    expect(p.blockMove("$")).toBe(true);
    expect(glyph().textContent).toBe("b");
    p.moveRows(1);
    expect(glyph().textContent).toBe("\u05d4");
    expect(p.block.index()).toBe(4);
  });
});

describe("the fake layout's characters", () => {
  it("answer a hit test only inside the view", () => {
    show("hello\n\nnext");
    const hit = (document as unknown as { caretRangeFromPoint(x: number, y: number): Range | null }).caretRangeFromPoint;
    expect(hit(9, 10)!.startOffset).toBe(1);
    expect(hit(13, 10)!.startOffset).toBe(2);
    expect(hit(-1, 10)).toBeNull();
    expect(hit(9, window.innerHeight + 1)).toBeNull();
  });

  it("leave a range over more than one character as it was without them", () => {
    show("hello world\n\nnext");
    const r = document.createRange();
    r.selectNodeContents(root.querySelector("p")!.firstChild!);
    expect([...r.getClientRects()].map((x) => [x.top, x.height, x.left, x.width])).toEqual([[0, 20, 0, 600]]);
  });
});

/* The blink: from the theme's three times, read at each draw and on a theme event. The off phase is a class on the
   block (`mdp-blink-hidden`, hidden by `chrome.css`). */
describe("the block's blink", () => {
  const VARS = ["--nv-cursor-blinkwait-ms", "--nv-cursor-blinkon-ms", "--nv-cursor-blinkoff-ms"];
  const setBlink = (wait: string | null, on: string | null, off: string | null) => {
    VARS.forEach((name, i) => {
      const v = [wait, on, off][i];
      if (v === null) document.documentElement.style.removeProperty(name);
      else document.documentElement.style.setProperty(name, v);
    });
  };
  const hiddenPhase = () => block().classList.contains("mdp-blink-hidden");
  /** Timers left once the page's own settle (the reveal throttle) has run: only a blink keeps one. */
  const timers = () => { vi.advanceTimersByTime(5000); return vi.getTimerCount(); };
  const reducedMotion = (on: boolean) => Object.defineProperty(window, "matchMedia", {
    configurable: true, value: (q: string) => ({ matches: on && q.includes("reduce") }),
  });
  afterEach(() => {
    setBlink(null, null, null);
    delete (window as { matchMedia?: unknown }).matchMedia;
  });

  it("is steady by default and with any time missing or 0ms", () => {
    show("hello world\n\nnext");
    p.onFocus(true);
    expect(timers()).toBe(0);
    for (const times of [[null, null, null], ["0ms", "300ms", "200ms"], ["600ms", "0ms", "200ms"], ["600ms", "300ms", "0ms"],
      ["600ms", "300ms", null], ["fast", "300ms", "200ms"]] as const) {
      setBlink(...times);
      p.drawBlock();
      expect(timers(), times.join()).toBe(0);
      expect(hiddenPhase()).toBe(false);
    }
  });

  it("blinks with all three set: shown for the wait, then off and on by the given times", () => {
    show("hello world\n\nnext");
    setBlink("600ms", "300ms", "200ms");
    p.onFocus(true);
    const at = (ms: number) => { vi.advanceTimersByTime(ms); return hiddenPhase(); };
    expect(hiddenPhase()).toBe(false);
    expect(at(599)).toBe(false);
    expect(at(1)).toBe(true);
    expect(at(199)).toBe(true);
    expect(at(1)).toBe(false);
    expect(at(299)).toBe(false);
    expect(at(1)).toBe(true);
    expect(at(200)).toBe(false);
    // The block keeps its place and size in the off phase: it is only invisible.
    expect(block().hidden).toBe(false);
  });

  it("hides the off phase with CSS, keeping the box", () => {
    sheet(readFileSync(resolve(here, "../style/chrome.css"), "utf8"));
    show("hello\n\nnext");
    p.onFocus(true);
    expect(getComputedStyle(block()).visibility).not.toBe("hidden");
    block().classList.add("mdp-blink-hidden");
    expect(getComputedStyle(block()).visibility).toBe("hidden");
  });

  it("restarts the wait at every move, and shows a block that was in its off phase", () => {
    show("hello world\n\nnext");
    setBlink("600ms", "300ms", "200ms");
    p.onFocus(true);
    vi.advanceTimersByTime(500);
    expect(p.blockMove("l")).toBe(true);
    vi.advanceTimersByTime(599);
    expect(hiddenPhase()).toBe(false);
    vi.advanceTimersByTime(1);
    expect(hiddenPhase()).toBe(true);
    vi.advanceTimersByTime(100);
    expect(p.blockMove("l")).toBe(true);
    expect(hiddenPhase()).toBe(false);
    vi.advanceTimersByTime(599);
    expect(hiddenPhase()).toBe(false);
    vi.advanceTimersByTime(1);
    expect(hiddenPhase()).toBe(true);
  });

  it("is not restarted by a draw that changed nothing", () => {
    show("hello world\n\nnext");
    setBlink("600ms", "300ms", "200ms");
    p.onFocus(true);
    vi.advanceTimersByTime(400);
    p.drawBlock();
    window.dispatchEvent(new Event("scroll"));
    vi.advanceTimersByTime(200);
    expect(hiddenPhase()).toBe(true);
  });

  it("follows onTheme: new times start again, 0ms is steady (the event itself: main.test.ts)", () => {
    show("hello world\n\nnext");
    p.onFocus(true);
    expect(timers()).toBe(0);
    setBlink("1000ms", "100ms", "50ms");
    p.onTheme();
    vi.advanceTimersByTime(999);
    expect(hiddenPhase()).toBe(false);
    vi.advanceTimersByTime(1);
    expect(hiddenPhase()).toBe(true);
    vi.advanceTimersByTime(50);
    expect(hiddenPhase()).toBe(false);
    setBlink("200ms", "100ms", "50ms");
    p.onTheme();
    vi.advanceTimersByTime(199);
    expect(hiddenPhase()).toBe(false);
    vi.advanceTimersByTime(1);
    expect(hiddenPhase()).toBe(true);
    setBlink("200ms", "0ms", "50ms");
    p.onTheme();
    expect(hiddenPhase()).toBe(false);
    expect(timers()).toBe(0);
  });

  it("sets no timer while the keys are away or the block has no row, and starts again with them", () => {
    show("hello\n\n---\n\nworld");
    setBlink("600ms", "300ms", "200ms");
    p.onFocus(true);
    expect(timers()).toBe(1);
    vi.advanceTimersByTime(650);
    expect(hiddenPhase()).toBe(true);
    p.onFocus(false);
    expect(hiddenPhase()).toBe(false);
    expect(timers()).toBe(0);
    p.onFocus(true);
    expect(timers()).toBe(1);
    // A row with no text: no block, no timer.
    p.moveRows(1);
    expect(block().hidden).toBe(true);
    expect(timers()).toBe(0);
    p.moveRows(1);
    expect(timers()).toBe(1);
    p.dispose();
    expect(timers()).toBe(0);
  });

  it("stays steady while the system asks for reduced motion", () => {
    show("hello world\n\nnext");
    setBlink("600ms", "300ms", "200ms");
    reducedMotion(true);
    p.onFocus(true);
    expect(timers()).toBe(0);
    reducedMotion(false);
    p.onTheme();
    expect(timers()).toBe(1);
    reducedMotion(true);
    p.onTheme();
    expect(timers()).toBe(0);
    expect(hiddenPhase()).toBe(false);
  });

  it("stops at its next phase when the system starts asking for reduced motion, with no draw", () => {
    show("hello world\n\nnext");
    setBlink("600ms", "300ms", "200ms");
    p.onFocus(true);
    vi.advanceTimersByTime(600);
    expect(hiddenPhase()).toBe(true);
    reducedMotion(true);
    vi.advanceTimersByTime(200);
    expect(hiddenPhase()).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });
});
