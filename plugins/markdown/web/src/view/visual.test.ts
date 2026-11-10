import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { editSelection, installKeys, yankSelection } from "./keys";
import { Preview } from "./preview";
import { drawnPart, selectionBoxes, sourceSelection } from "./selection";

/* The fake layout (fakelayout.ts): each tagged leaf block 20 px a source line, a 10 px gap between blocks; a range
   over a text reports its block's rows. */
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const FENCE = "`".repeat(3);
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 200, top: 1,
    top_fraction: 0, bottom: 40, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}

let layout: FakeLayout;
let p: Preview;
let root: HTMLElement;
let calls: { verb: string; args: unknown }[];
let answer: (verb: string, args: unknown) => Promise<unknown>;
let installed: InstallOptions | null;
const kit = { version: 5, install: vi.fn(async (o: InstallOptions) => { installed = o; return () => {}; }), setCurrent: vi.fn(),
  message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []), findRefresh: vi.fn(), findSeed: vi.fn(), visualMode: vi.fn(() => null) };
const jumps = () => calls.filter((c) => c.verb === "editor.jump").map((c) => c.args);

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root);
  root.style.lineHeight = "20px";
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  kit.message.mockClear();
  installed = null;
  calls = [];
  answer = async () => ({ applied: true });
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return answer(verb, args); }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => {
  p.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks();
  delete (window as unknown as { eitriKeys?: unknown }).eitriKeys;
});

function show(body: string, v: Partial<EditorView> = {}): void {
  p.onView(view(v));
  p.onText({ buf: 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}

/** The text node holding `text` under the root, and the offset of `text` in it. */
function find(text: string, nth = 0): { node: Text; offset: number } {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let seen = 0;
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const at = (n as Text).data.indexOf(text);
    if (at >= 0 && seen++ === nth) return { node: n as Text, offset: at };
  }
  throw new Error(`no text ${text}`);
}
/** Selects from the start of `from` to the end of `to` (backward with `back`). */
function select(from: string, to: string, back = false, nth = 0): Selection {
  const a = find(from, nth);
  const b = find(to, nth);
  const s = document.getSelection()!;
  if (back) s.setBaseAndExtent(b.node, b.offset + to.length, a.node, a.offset);
  else s.setBaseAndExtent(a.node, a.offset, b.node, b.offset + to.length);
  return s;
}

describe("the source of a selection", () => {
  it("is charwise inside one source line when its text occurs there once, columns in characters", () => {
    show("# 标题\n\n中文 段落 with **bold** words\n");
    expect(sourceSelection(p, select("bold", "bold"), "char")).toEqual(
      { mode: "char", anchor: { line: 2, col: 14 }, focus: { line: 2, col: 17 } });
    expect(sourceSelection(p, select("段落", "段落"), "char")).toEqual(
      { mode: "char", anchor: { line: 2, col: 4 }, focus: { line: 2, col: 5 } });
  });

  it("keeps the selection's direction: the anchor where it started, the focus at its cursor", () => {
    show("one two three\n");
    expect(sourceSelection(p, select("two", "three", true), "char")).toEqual(
      { mode: "char", anchor: { line: 0, col: 13 }, focus: { line: 0, col: 5 } });
  });

  it("finds the source line of a wrapped paragraph's text by its words, not by its height", () => {
    // Three source lines of one paragraph: the fake layout gives each 20 px, so a selection's height cannot tell
    // which line it is on; its text can.
    show("alpha beta\ngamma delta\nepsilon zeta\n");
    expect(sourceSelection(p, select("epsilon", "epsilon"), "char")).toEqual(
      { mode: "char", anchor: { line: 2, col: 1 }, focus: { line: 2, col: 7 } });
    expect(sourceSelection(p, select("delta", "epsilon"), "char")).toEqual(
      { mode: "line", anchor: { line: 1, col: 0 }, focus: { line: 2, col: 0 } });
  });

  it("never matches a drawn word inside an earlier line's link target or reference definition", () => {
    show("see [x](next) and [y][ref]\nnext word here\n\n[ref]: https://example.com/word\n");
    expect(sourceSelection(p, select("next", "next"), "char")).toEqual(
      { mode: "char", anchor: { line: 1, col: 1 }, focus: { line: 1, col: 4 } });
    expect(sourceSelection(p, select("word", "word"), "char")).toEqual(
      { mode: "char", anchor: { line: 1, col: 6 }, focus: { line: 1, col: 9 } });
  });

  it("finds a word after an autolink on the autolink's own line: its address is drawn", () => {
    show("See <https://a.io> now\nthen more text now\n");
    expect(sourceSelection(p, select("now", "now"), "char")).toEqual(
      { mode: "char", anchor: { line: 0, col: 20 }, focus: { line: 0, col: 22 } });
    show("Mail <me@x.org> soon\nlater soon\n");
    expect(sourceSelection(p, select("soon", "soon"), "char")).toEqual(
      { mode: "char", anchor: { line: 0, col: 17 }, focus: { line: 0, col: 20 } });
  });

  it("never takes a word from an HTML comment or an image's alt text on the line before", () => {
    show("start <!-- word -->\nword end\n");
    expect(sourceSelection(p, select("word", "word"), "char")).toEqual(
      { mode: "char", anchor: { line: 1, col: 1 }, focus: { line: 1, col: 4 } });
    show("pic ![word](a.png)\nword end\n");
    expect(sourceSelection(p, select("word", "word"), "char")).toEqual(
      { mode: "char", anchor: { line: 1, col: 1 }, focus: { line: 1, col: 4 } });
  });

  it("keeps what looks like a tag inside a code span or a fenced block: it is drawn", () => {
    show("`<details>` here\nmore here\n");
    expect(sourceSelection(p, select("here", "here"), "char")).toEqual(
      { mode: "char", anchor: { line: 0, col: 13 }, focus: { line: 0, col: 16 } });
    show(`${FENCE}\n<div> a\nb <span>\n${FENCE}\n`);
    expect(sourceSelection(p, select("b", "b"), "char")).toEqual(
      { mode: "char", anchor: { line: 2, col: 1 }, focus: { line: 2, col: 1 } });
  });

  it("reads a link target holding backticks as a target, and an undefined image reference as text", () => {
    show("one [x](https://a/`ghost`)\nghost end\n");
    expect(sourceSelection(p, select("ghost", "ghost"), "char")).toEqual(
      { mode: "char", anchor: { line: 1, col: 1 }, focus: { line: 1, col: 5 } });
    show("one ![ghost] later\n");
    expect(sourceSelection(p, select("ghost", "ghost"), "char")).toEqual(
      { mode: "char", anchor: { line: 0, col: 7 }, focus: { line: 0, col: 11 } });
    expect(drawnPart("![ghost] and [y][ref] and [z][none]", false, new Set(["ghost", "ref"])))
      .toBe(" and [y] and [z][none]");
  });

  it("reads a line in time that grows with its length, unmatched backticks too", () => {
    vi.useRealTimers();
    const time = (n: number) => {
      const line = `prefix ${"`".repeat(n)} x ${"` ".repeat(n)}`;
      let best = Number.POSITIVE_INFINITY;
      for (let i = 0; i < 3; i++) {
        const t0 = performance.now();
        drawnPart(line);
        best = Math.min(best, performance.now() - t0);
      }
      return Math.max(best, 0.05);
    };
    time(1000);
    // Sixteen times the text: a linear reading takes about sixteen times as long, a quadratic one 256.
    expect(time(32000) / time(2000)).toBeLessThan(64);
  });


  it.each([
    (n: number) => "![".repeat(n),
    (n: number) => "![".repeat(n) + "]",
    (n: number) => "![a".repeat(n) + "]",
    (n: number) => "![]( (".repeat(n),
    (n: number) => "]( (".repeat(n),
  ])("reads repeated unmatched image and link markers in time proportional to the line length: %s", (input) => {
    vi.useRealTimers();
    const time = (n: number) => {
      const line = input(n);
      let best = Number.POSITIVE_INFINITY;
      for (let i = 0; i < 3; i++) {
        const start = performance.now();
        expect(drawnPart(line)).toBe(line);
        best = Math.min(best, performance.now() - start);
      }
      return Math.max(best, 0.05);
    };
    time(1000);
    expect(time(32000) / time(2000)).toBeLessThan(64);
    expect(drawnPart("![![open then ![closed](x.png)")).toBe("");
    expect(drawnPart("![open `![code` rest")).toBe("![open ![code rest");
  });

  it("caps shortcut and collapsed image labels while keeping long inline and full-reference alt text", () => {
    const capped = "a".repeat(999);
    const over = capped + "a";
    const refs = new Set([capped, over, "ok"]);
    expect(drawnPart(`![${capped}] ![${capped}][]`, false, refs)).toBe(" ");
    expect(drawnPart(`![${over}]`, false, refs)).toBe(`![${over}]`);
    expect(drawnPart(`![${over}][]`, false, refs)).toBe(`![${over}]`);
    expect(drawnPart(`![${over}](image.png) ![${over}][ok]`, false, refs)).toBe(" ");
    expect(drawnPart(`![${capped}][missing]`, false, refs)).toBe(`![${capped}][missing]`);
  });

  it("is linewise when the word's one written copy is not the one drawn (an entity, a link's target)", () => {
    show("caf&eacute; [x](caf\u00e9)\n");
    expect(sourceSelection(p, select("caf\u00e9", "caf\u00e9"), "char")).toEqual(
      { mode: "line", anchor: { line: 0, col: 0 }, focus: { line: 0, col: 0 } });
  });

  it("is linewise on its line when its text occurs there twice", () => {
    show("x the y the z\n");
    expect(sourceSelection(p, select("the", "the"), "char")).toEqual(
      { mode: "line", anchor: { line: 0, col: 0 }, focus: { line: 0, col: 0 } });
  });

  it("is linewise over the lines it spans across blocks, and always linewise from V", () => {
    show("first para\n\n- item one\n- item two\n\nlast para\n");
    expect(sourceSelection(p, select("para", "two"), "char")).toEqual(
      { mode: "line", anchor: { line: 0, col: 0 }, focus: { line: 3, col: 0 } });
    expect(sourceSelection(p, select("item", "one"), "line")).toEqual(
      { mode: "line", anchor: { line: 2, col: 0 }, focus: { line: 2, col: 0 } });
  });

  it("reads a fenced block's code lines without its fences", () => {
    show(`intro\n\n${FENCE}\nconst a = 1;\nconst b = 2;\n${FENCE}\n`);
    expect(sourceSelection(p, select("b = 2", "b = 2"), "char")).toEqual(
      { mode: "char", anchor: { line: 4, col: 7 }, focus: { line: 4, col: 11 } });
  });

  it("ends at the last character selected, not at the start of the block after it", () => {
    show("first para\n\nsecond para\n");
    const a = find("first");
    const b = find("second");
    const s = document.getSelection()!;
    s.setBaseAndExtent(a.node, a.offset, b.node, 0);
    expect(sourceSelection(p, s, "line")).toEqual({ mode: "line", anchor: { line: 0, col: 0 }, focus: { line: 0, col: 0 } });
  });
});

describe("visual mode's Enter", () => {
  it("hands the selection to nvim with the keys, nvim's cursor at the selection's cursor", async () => {
    show("one two three\n\nfour five\n");
    p.onFocus(true);
    select("two", "five");
    editSelection(p, "char");
    await vi.runAllTimersAsync();
    expect(jumps()).toEqual([
      { buf: 1, line: 1, col: 0, focus: true, select: { line: 3, col: 0, mode: "line" } },
    ]);
    expect(kit.message).not.toHaveBeenCalled();
  });

  it("says so when nvim did not take it, and a host without select gets the plain jump to the cursor", async () => {
    show("one two three\n");
    p.onFocus(true);
    answer = async () => ({ applied: false });
    select("two", "two");
    editSelection(p, "char");
    await vi.runAllTimersAsync();
    expect(kit.message).toHaveBeenCalledWith(expect.stringContaining("did not take the selection"));
    answer = async (_verb, args) => {
      if ((args as { select?: unknown }).select) throw Object.assign(new Error("unknown field select"), { code: "bad_request" });
      return { applied: true };
    };
    calls = [];
    select("two", "two");
    editSelection(p, "char");
    await vi.runAllTimersAsync();
    expect(jumps()).toEqual([
      { buf: 1, line: 1, col: 5, focus: true, select: { line: 1, col: 7, mode: "char" } },
      { buf: 1, line: 1, col: 7, focus: true },
    ]);
  });

  it("holds back the page's reveals while the keys go to nvim", async () => {
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"));
    p.onFocus(true);
    p.moveRows(1);
    await vi.advanceTimersByTimeAsync(100);
    const from = calls.length;
    select("p1", "p1");
    editSelection(p, "char");
    p.moveRows(1);
    await vi.advanceTimersByTimeAsync(200);
    expect(jumps().length).toBe(1);
    expect(calls.slice(from).filter((c) => c.verb === "editor.reveal" && (c.args as { cursor?: number }).cursor !== undefined)
      .length).toBe(0);
  });
});

describe("visual mode's Y", () => {
  it("copies the Markdown of the source lines the selection spans", async () => {
    const written: string[] = [];
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async (t: string) => { written.push(t); } } });
    show("first **para**\n\n- item one\n- item two\n\nlast\n");
    select("para", "one");
    yankSelection(p, "char");
    await vi.runAllTimersAsync();
    expect(written).toEqual(["first **para**\n\n- item one"]);
    expect(kit.message).toHaveBeenCalledWith("yanked 3 lines");
    delete (navigator as unknown as { clipboard?: unknown }).clipboard;
  });
});

describe("the page's visual hooks", () => {
  it("ask the kit after every render, so a selection whose text was replaced ends then", async () => {
    show("one\n");
    await installKeys(p, { run: () => false });
    kit.visualMode.mockClear();
    p.onText({ buf: 1, tick: 2, name: "/p/notes.md", text: "two\n" });
    vi.advanceTimersByTime(0);
    expect(kit.visualMode).toHaveBeenCalled();
  });

  it("start at the cursor's row and follow the selection's cursor with the page's", async () => {
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"));
    await installKeys(p, { run: () => false });
    p.onFocus(true);
    p.moveRows(1);
    p.moveRows(1);
    const start = installed!.visual!.start!()!;
    expect((start.startContainer as Text).data).toBe("p2");
    const p30 = find("p30");
    const range = document.createRange();
    range.setStart(p30.node, 0);
    range.setEnd(p30.node, 1);
    installed!.visual!.follow!(range.getBoundingClientRect());
    // p30 is at 900, past the bottom margin: the page scrolls as for a jump, and the cursor is on its row.
    expect(p.cursorBand()!.top).toBe(900);
    expect(window.scrollY).toBeGreaterThan(0);
    expect(Object.keys(installed!.visual!.keys!)).toEqual(["<CR>", "Y"]);
  });
});

describe("visual line mode's band", () => {
  const band = () => document.querySelector<HTMLElement>(".mdp-visual-band")!;
  const TABLE = "intro\n\n| crate | lines |\n| --- | --- |\n| core | 146,507 |\n| shell | 48,315 |\n| panel | 22,486 |\n";
  afterEach(() => { document.documentElement.removeAttribute("data-eitri-visual"); });
  const box = (el: Element) => layout.boxOf(el)!;
  const rowWith = (text: string) => [...root.querySelectorAll("tr")].find((r) => r.textContent?.includes(text))!;

  it("covers a table row's whole box, every cell, when V is on it", () => {
    show(TABLE);
    document.documentElement.setAttribute("data-eitri-visual", "line");
    select("shell", "48,315");
    p.placeVisual();
    const row = box(rowWith("shell"));
    expect(band().hidden).toBe(false);
    expect(band().style.top).toBe(`${row.top}px`);
    expect(band().style.height).toBe(`${row.height}px`);
  });

  it("is one band over a range of rows, the gaps between them included", () => {
    show(TABLE);
    document.documentElement.setAttribute("data-eitri-visual", "line");
    select("core", "48,315");
    p.placeVisual();
    const first = box(rowWith("core"));
    const last = box(rowWith("shell"));
    expect(band().style.top).toBe(`${first.top}px`);
    expect(band().style.height).toBe(`${last.top + last.height - first.top}px`);
    expect(document.querySelectorAll(".mdp-visual-band").length).toBe(1);
  });

  it("spans paragraphs, list items and a quote from the first selected row to the last", () => {
    show("first para\n\n- one\n- two\n\n> quoted\n");
    document.documentElement.setAttribute("data-eitri-visual", "line");
    select("first", "quoted");
    p.placeVisual();
    const a = box(find("first").node.parentElement!);
    const b = box(find("quoted").node.parentElement!);
    expect(band().style.top).toBe(`${a.top}px`);
    expect(band().style.height).toBe(`${b.top + b.height - a.top}px`);
  });

  it("is not drawn charwise, without a selection on the page, or when the mode ended", () => {
    show(TABLE);
    document.documentElement.setAttribute("data-eitri-visual", "char");
    select("shell", "48,315");
    p.placeVisual();
    expect(band().hidden).toBe(true);
    document.documentElement.setAttribute("data-eitri-visual", "line");
    document.getSelection()!.removeAllRanges();
    p.placeVisual();
    expect(band().hidden).toBe(true);
  });

  it("follows the selection: the kit's follow after a move places it", () => {
    show(TABLE);
    document.documentElement.setAttribute("data-eitri-visual", "line");
    select("core", "146,507");
    const r = box(rowWith("core"));
    p.followSelection({ top: r.top, bottom: r.top + r.height, left: 0, right: 10, width: 10, height: r.height, x: 0,
      y: r.top, toJSON: () => ({}) } as DOMRect);
    expect(band().hidden).toBe(false);
    expect(band().style.top).toBe(`${r.top}px`);
  });
});

describe("charwise visual mode's boxes", () => {
  /* Boxes by text: each text node named here lies on one line at `top`, `left`, `width` px (`rows` lines of it
     stacked 20 px apart, the width split evenly), its characters evenly wide; text not named keeps the fake layout. */
  type Fake = { top: number; height: number; left: number; width: number; rows?: number };
  let boxes: Map<string, Fake>;
  let undo: () => void = () => {};
  beforeEach(() => {
    boxes = new Map();
    const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
    const had = Object.getOwnPropertyDescriptor(proto, "getClientRects")!;
    Object.defineProperty(proto, "getClientRects", {
      configurable: true,
      value: function (this: Range) {
        const node = this.startContainer;
        const f = node.nodeType === Node.TEXT_NODE ? boxes.get((node as Text).data) : undefined;
        if (!f) return (had.value as () => DOMRect[]).call(this);
        const n = (node as Text).data.length;
        const rows = f.rows ?? 1;
        const per = Math.ceil(n / rows);
        const out: DOMRect[] = [];
        const end = this.endContainer === node ? this.endOffset : n;
        for (let r = 0; r < rows; r++) {
          const a = Math.max(this.startOffset, r * per);
          const b = Math.min(end, (r + 1) * per);
          if (b < a) continue;
          const w = f.width / per;
          const top = f.top + 20 * r - window.scrollY;
          const left = f.left + (a - r * per) * w;
          out.push({ top, height: f.height, bottom: top + f.height, left, right: left + (b - a) * w, width: (b - a) * w,
            x: left, y: top, toJSON: () => ({}) } as DOMRect);
        }
        return out;
      },
    });
    undo = () => Object.defineProperty(proto, "getClientRects", had);
  });
  afterEach(() => { undo(); document.documentElement.removeAttribute("data-eitri-visual"); });
  const drawn = () => [...document.querySelectorAll<HTMLElement>(".mdp-visual-box")]
    .map((b) => [b.style.top, b.style.height, b.style.left, b.style.width].map((v) => parseFloat(v)));

  it("draws each table cell's selected text apart, every box of a row at the row's height", () => {
    show("| crate | lines | tests |\n| --- | --- | --- |\n| shell | 48,315 | 57,077 |\n");
    boxes.set("shell", { top: 102, height: 18, left: 10, width: 50 });
    boxes.set("48,315", { top: 100, height: 22, left: 90, width: 60 });
    boxes.set("57,077", { top: 100, height: 22, left: 180, width: 60 });
    const range = document.createRange();
    const a = find("shell"), b = find("57,077");
    range.setStart(a.node, a.offset);
    range.setEnd(b.node, 1);
    expect(selectionBoxes(range, 8)).toEqual([
      { top: 100, bottom: 122, left: 10, right: 60 },
      { top: 100, bottom: 122, left: 90, right: 150 },
      { top: 100, bottom: 122, left: 180, right: 190 },
    ]);
  });

  it("joins a row's text across a code span's padding and a space between two spans", () => {
    show("x\n");
    const para = root.querySelector("p")!;
    para.replaceChildren();
    const a = document.createElement("span"); a.textContent = "fn";
    const b = document.createElement("span"); b.textContent = "main";
    para.append(a, document.createTextNode(" "), b);
    boxes.set("fn", { top: 0, height: 18, left: 0, width: 16 });
    boxes.set(" ", { top: 0, height: 18, left: 16, width: 8 });
    boxes.set("main", { top: 0, height: 18, left: 24, width: 32 });
    const range = document.createRange();
    range.selectNodeContents(para);
    expect(selectionBoxes(range, 7)).toEqual([{ top: 0, bottom: 18, left: 0, right: 56 }]);
  });

  it("joins a row's text across a code span's padding and leaves out what has no width", () => {
    show("say `code` here\n");
    boxes.set("say ", { top: 0, height: 18, left: 0, width: 40 });
    boxes.set("code", { top: 0, height: 18, left: 46, width: 40 });
    boxes.set(" here", { top: 0, height: 18, left: 92, width: 50 });
    const range = document.createRange();
    range.selectNodeContents(root.querySelector("p")!);
    expect(selectionBoxes(range, 8)).toEqual([{ top: 0, bottom: 18, left: 0, right: 142 }]);
  });

  it("is drawn by the page charwise, a row's band tall, so a wrapped paragraph's rows meet; not linewise", () => {
    show("a paragraph long enough to wrap\n");
    boxes.set("a paragraph long enough to wrap", { top: 1, height: 18, left: 0, width: 300, rows: 2 });
    document.documentElement.setAttribute("data-eitri-visual", "char");
    select("paragraph", "enough");
    p.placeVisual();
    const got = drawn();
    expect(got.length).toBe(2);
    expect(got[0][0] + got[0][1]).toBe(got[1][0]);
    document.documentElement.setAttribute("data-eitri-visual", "line");
    p.placeVisual();
    expect(drawn()).toEqual([]);
    expect(document.querySelector<HTMLElement>(".mdp-visual-band")!.hidden).toBe(false);
  });

  it("cuts a box to the sides of a block that scrolls sideways", () => {
    show("```\nwide line\n```\n");
    boxes.set("wide line\n", { top: 0, height: 18, left: 500, width: 300 });
    const range = document.createRange();
    range.selectNodeContents(root.querySelector("code")!);
    expect(root.querySelector("pre")!.classList.contains("mdp-wide")).toBe(true);
    expect(selectionBoxes(range, 8)).toEqual([{ top: 0, bottom: 18, left: 500, right: 600 }]);
  });

  it("moves with a block scrolled sideways under it", () => {
    show("| a | b |\n| --- | --- |\n| cell | other |\n");
    boxes.set("cell", { top: 40, height: 18, left: 10, width: 40 });
    document.documentElement.setAttribute("data-eitri-visual", "char");
    select("cell", "cell");
    p.placeVisual();
    expect(drawn()[0][2]).toBe(10);
    boxes.set("cell", { top: 40, height: 18, left: -30, width: 40 });
    root.querySelector("table")!.dispatchEvent(new Event("scroll"));
    expect(drawn()[0][2]).toBe(0);
  });

  it("reads the rows once for a long selection, not once a box", () => {
    const lines = Array.from({ length: 300 }, (_, i) => `line ${i}`);
    show("```\n" + lines.join("\n") + "\n```\n");
    document.documentElement.setAttribute("data-eitri-visual", "char");
    select("line 0", "line 299");
    const near = vi.spyOn(Preview.prototype as unknown as { rowsNear(y: number): unknown }, "rowsNear");
    p.placeVisual();
    expect(drawn().length).toBe(300);
    expect(near.mock.calls.length).toBeLessThanOrEqual(2);
  });

  it("hides the kit's highlight: the page draws the selection itself", async () => {
    const { readFileSync } = await import("node:fs");
    const { dirname, resolve } = await import("node:path");
    const { fileURLToPath } = await import("node:url");
    const css = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../style/chrome.css"), "utf8");
    expect(css).toMatch(/html\[data-eitri-visual\] ::highlight\(eitri-visual\) \{\s*background-color: transparent;/);
  });
});
