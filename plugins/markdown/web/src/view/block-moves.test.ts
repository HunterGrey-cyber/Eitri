import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { installKeys } from "./keys";
import { Preview } from "./preview";

/* The block's moves, in the fake layout with characters (`block.test.ts` says how they are laid out: from x 0 in rows
   of 20 px, 8 px for most letters, 6 for i l r f t j, 4 for a space, 20 for CJK). Paragraphs of one row are 30 px
   apart; text with TALL in it wraps onto six rows. The kit is a stub: `motion` answers what each test says. */
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 200, top: 1,
    top_fraction: 0, bottom: 40, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}
type Place = { node: Node; offset: number };
type Motion = (at: Place, motion: string) => Place | null;

let layout: FakeLayout;
let p: Preview;
let root: HTMLElement;
let calls: { verb: string; args: unknown }[];
let installed: InstallOptions | null;
let motion: Motion;
let motionCalls: [Place, string][];
const removers: (() => void)[] = [];
const kitStub = {
  version: 6, install: vi.fn(async (o: InstallOptions) => { installed = o; return () => {}; }), setCurrent: vi.fn(),
  message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []), findRefresh: vi.fn(), findSeed: vi.fn(),
  motion: (at: Place, m: string) => { motionCalls.push([at, m]); return motion(at, m); },
};
const reveals = () => calls.filter((c) => c.verb === "editor.reveal");
const glyph = () => p.block.el.querySelector<HTMLElement>(".mdp-cursor-glyph")!.textContent;
const shown = () => !p.block.el.hidden;
const bandTop = () => p.cursorBand()!.top;
const press = (action: string) => installed!.run(action, new KeyboardEvent("keydown"));

beforeEach(async () => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.head.innerHTML = "";
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root, { chars: true });
  root.style.lineHeight = "20px";
  root.style.fontSize = "16px";
  calls = [];
  motionCalls = [];
  motion = () => null;
  installed = null;
  (window as unknown as { eitriKeys: typeof kitStub }).eitriKeys = kitStub;
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied: true }; }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => {
  for (const remove of removers.splice(0)) remove();
  p.dispose();
  layout.restore();
  delete (window as { eitriKeys?: unknown }).eitriKeys;
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function show(body: string): void {
  p.onView(view());
  p.onText({ buf: 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}
async function keys(): Promise<void> { removers.push(await installKeys(p, { run: () => false })); }
/** A place before the character `offset` of the first text node holding `text`. */
function placeIn(text: string, offset = 0): Place {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode() as Text | null; n; n = walker.nextNode() as Text | null) {
    const i = n.data.indexOf(text);
    if (i >= 0) return { node: n, offset: i + offset };
  }
  throw new Error(`no text ${text}`);
}
const rowTop = (el: Element) => layout.boxOf(el)!.top;
const l = (n: number) => { for (let i = 0; i < n; i++) p.blockMove("l"); };
const paras = (r: string[]) => r.join("\n\n");

describe("h and l", () => {
  it("stop at the element's ends", () => {
    show("hello\n\nworld");
    p.onFocus(true);
    l(4);
    expect(glyph()).toBe("o");
    expect(p.blockMove("l")).toBe(false);
    expect(glyph()).toBe("o");
    for (let i = 0; i < 4; i++) p.blockMove("h");
    expect(glyph()).toBe("h");
    expect(p.blockMove("h")).toBe(false);
    expect(bandTop()).toBe(0);
  });

  it("stop at a code line's end and at a list item's own text", () => {
    show("```\nab\ncd\n```\n\n- up\n  - down");
    p.onFocus(true);
    expect(glyph()).toBe("a");
    expect(p.blockMove("l")).toBe(true);
    expect(glyph()).toBe("b");
    // The line break is not a character: `c` is on the next row.
    expect(p.blockMove("l")).toBe(false);
    expect(glyph()).toBe("b");
    p.jumpCursorTo(rowTop(root.querySelector("li")!) + 1);
    expect(glyph()).toBe("u");
    l(5);
    // `up` ends the item's own text; the nested item is another element.
    expect(glyph()).toBe("p");
  });

  it("request no reveal inside a row, and one when the block lands on another row", () => {
    show("hello world\n\nnext");
    p.onFocus(true);
    vi.advanceTimersByTime(2000);
    calls.length = 0;
    l(3);
    vi.advanceTimersByTime(2000);
    expect(reveals()).toEqual([]);
    expect(p.block.index()).toBe(3);
    motion = () => placeIn("next");
    expect(p.blockMove("w")).toBe(true);
    vi.advanceTimersByTime(2000);
    expect(reveals().length).toBe(1);
  });

  it("do nothing on a row without text", () => {
    show("hello\n\n---\n\nworld");
    p.onFocus(true);
    p.moveRows(1);
    expect(shown()).toBe(false);
    expect([p.blockMove("l"), p.blockMove("h"), p.blockMove("0"), p.blockMove("$")]).toEqual([false, false, false, false]);
    expect(bandTop()).toBe(30);
  });
});

describe("w b e", () => {
  it("land where the kit's motion says, asked from the block's character", () => {
    show("hello world\n\nnext");
    p.onFocus(true);
    const world = placeIn("world");
    motion = () => world;
    expect(p.blockMove("w")).toBe(true);
    expect(glyph()).toBe("w");
    expect(motionCalls).toEqual([[placeIn("hello"), "w"]]);
    motion = () => placeIn("hello", 4);
    expect(p.blockMove("b")).toBe(true);
    expect(glyph()).toBe("o");
    expect(motionCalls[1][1]).toBe("b");
    motion = () => placeIn("world", 4);
    p.blockMove("e");
    expect(glyph()).toBe("d");
    expect(motionCalls[2]).toEqual([placeIn("hello", 4), "e"]);
    expect(bandTop()).toBe(0);
  });

  it("cross into the next block, and the band follows", () => {
    show("hello\n\nworld");
    p.onFocus(true);
    motion = () => placeIn("world");
    expect(p.blockMove("w")).toBe(true);
    expect(bandTop()).toBe(30);
    expect(glyph()).toBe("w");
    motion = () => placeIn("hello");
    expect(p.blockMove("b")).toBe(true);
    expect(bandTop()).toBe(0);
  });

  it("stay on the line's last character for a place at the line's end", () => {
    show("hello\n\nworld");
    p.onFocus(true);
    // An element place: after the paragraph's last child.
    const first = root.querySelector("p")!;
    motion = () => ({ node: first, offset: first.childNodes.length });
    p.blockMove("e");
    expect(glyph()).toBe("o");
    expect(bandTop()).toBe(0);
    // A place before a character that draws nothing: a code line's line break.
    show("```\nab\ncd\n```");
    p.onFocus(false);
    p.onFocus(true);
    expect(glyph()).toBe("a");
    motion = () => placeIn("ab", 2);
    expect(p.blockMove("e")).toBe(true);
    expect(glyph()).toBe("b");
    // The same place again: nothing to move.
    expect(p.blockMove("e")).toBe(false);
    expect(glyph()).toBe("b");
  });

  it("do nothing when the kit has no place", () => {
    show("hello\n\nworld");
    p.onFocus(true);
    motion = () => null;
    expect(p.blockMove("w")).toBe(false);
    expect(glyph()).toBe("h");
  });
});

describe("0 and $", () => {
  const long = `TALL ${"x".repeat(100)}`;

  it("go to the first and the last character of the row", () => {
    show(long);
    p.onFocus(true);
    expect(p.blockMove("$")).toBe(true);
    // 75 characters fill the first row of 600 px.
    expect(p.block.index()).toBe(74);
    expect(bandTop()).toBe(0);
    expect(p.blockMove("0")).toBe(true);
    expect(p.block.index()).toBe(0);
    expect(glyph()).toBe("T");
    p.moveRows(1);
    expect(bandTop()).toBe(20);
    p.blockMove("$");
    expect(p.block.index()).toBe(29);
    p.blockMove("0");
    expect(p.block.index()).toBe(0);
  });

  it("step across the wrap with l: the band follows to the next row", () => {
    show(long);
    p.onFocus(true);
    p.blockMove("$");
    expect(p.blockMove("l")).toBe(true);
    expect(bandTop()).toBe(20);
    expect(p.block.index()).toBe(0);
    expect(p.blockMove("h")).toBe(true);
    expect(bandTop()).toBe(0);
    expect(p.block.index()).toBe(74);
  });

  it("keep the row's end for j and k", () => {
    show(paras(["abcdeghk", "mn", "opqsuvwxyz"]));
    p.onFocus(true);
    p.blockMove("$");
    p.moveRows(1);
    expect(glyph()).toBe("n");
    p.moveRows(1);
    expect(glyph()).toBe("z");
  });
});

describe("j and k keep the block's x", () => {
  it("find the character at that x on each row, or the row's last", () => {
    show(paras(["abcdeghk", "mn", "opqsuvwxyz"]));
    p.onFocus(true);
    l(4);
    expect(glyph()).toBe("e");
    p.moveRows(1);
    expect(glyph()).toBe("n");
    p.moveRows(1);
    expect(glyph()).toBe("u");
    p.moveRows(-1);
    expect(glyph()).toBe("n");
    p.moveRows(-1);
    expect(glyph()).toBe("e");
  });

  it("start again from a horizontal move", () => {
    show(paras(["abcdeghk", "mn", "opqsuvwxyz"]));
    p.onFocus(true);
    l(4);
    p.moveRows(1);
    p.blockMove("h");
    expect(glyph()).toBe("m");
    p.moveRows(1);
    expect(glyph()).toBe("o");
  });

  it("read the x from geometry in text that is several nodes", () => {
    show(paras(["ab **cd** ef gh", "pq `rs` tu vw"]));
    p.onFocus(true);
    // `ab cd ef`: the `f` is the eighth character, at 48..54 (a narrow letter), its middle 51.
    l(7);
    expect(glyph()).toBe("f");
    p.moveRows(1);
    // `pq rs tu`: x 51 is in the `u` (44..52), in a third text node of its own.
    expect(glyph()).toBe("u");
  });

  it("with the target row below the view, never ask the hit test", () => {
    const rows = ["xxxxxxxxxxxx", ...Array.from({ length: 59 }, () => "i".repeat(20))];
    show(paras(rows));
    p.onFocus(true);
    l(5);
    expect(p.block.index()).toBe(5);
    const doc = document as unknown as { caretRangeFromPoint: (x: number, y: number) => Range | null };
    const hit = vi.spyOn(doc, "caretRangeFromPoint");
    const height = window.innerHeight;
    Object.defineProperty(window, "innerHeight", { value: 60, configurable: true, writable: true });
    try {
      p.fullPage(1);
      // x 44 is the `i` at 42..48: the eighth, where the first row's own index would be the sixth.
      expect(p.block.index()).toBe(7);
      // The row is at or below the view's bottom (60 px), where the hit test answers nothing.
      expect(bandTop()).toBeGreaterThanOrEqual(60);
      p.halfPage(1);
      expect(p.block.index()).toBe(7);
      p.halfPage(-1);
      p.fullPage(-1);
      expect(hit).not.toHaveBeenCalled();
    } finally {
      Object.defineProperty(window, "innerHeight", { value: height, configurable: true, writable: true });
    }
  });

  it("with half pages, as with rows", () => {
    show(paras(["xxxxxxxxxxxx", ...Array.from({ length: 59 }, () => "i".repeat(20))]));
    p.onFocus(true);
    l(5);
    p.halfPage(1);
    expect(p.block.index()).toBe(7);
    expect(bandTop()).toBeGreaterThan(300);
  });
});

describe("jumps land on the first non-blank", () => {
  const doc = "```\n    first\n```\n\nbody text\n\n```\n   last\n```";

  it("gg, G, } and { put the block on the row's first non-blank character", async () => {
    show(doc);
    await keys();
    p.onFocus(true);
    // The column held from `j` does not follow a jump.
    p.moveRows(1);
    expect(shown()).toBe(true);
    press("cursor.last");
    expect(glyph()).toBe("l");
    expect(p.block.index()).toBe(3);
    press("cursor.first");
    expect(glyph()).toBe("f");
    expect(p.block.index()).toBe(4);
    press("block.next");
    expect(glyph()).toBe("b");
    press("block.next");
    expect(glyph()).toBe("l");
    press("block.prev");
    expect(glyph()).toBe("b");
  });

  it("an anchor's jump and a history entry do as well", () => {
    show(doc);
    p.onFocus(true);
    l(2);
    p.jumpCursorTo(rowTop(root.querySelectorAll("pre")[1]) + 1);
    expect([glyph(), p.block.index()]).toEqual(["l", 3]);
    p.blockMove("l");
    p.jumpCursorToLine(0);
    expect([glyph(), p.block.index()]).toEqual(["f", 4]);
  });

  it("]] and [[ too", async () => {
    show("# one\n\nbody text\n\n## two words\n\nmore");
    await keys();
    p.onFocus(true);
    p.moveRows(1);
    l(3);
    press("heading.next");
    expect(glyph()).toBe("t");
    expect(p.block.index()).toBe(0);
    press("heading.prev");
    expect(glyph()).toBe("o");
  });
});

describe("a find and a click", () => {
  it("a find's match gets the block on its first character", async () => {
    show("hello world\n\nsecond paragraph here");
    await keys();
    p.onFocus(true);
    installed!.onJump!("before");
    const range = document.createRange();
    const at = placeIn("paragraph");
    range.setStart(at.node, at.offset);
    range.setEnd(at.node, at.offset + 9);
    const selection = document.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);
    installed!.onJump!("after");
    expect(bandTop()).toBe(30);
    expect(glyph()).toBe("p");
    expect(p.block.index()).toBe(7);
  });

  it("a click puts it on the clicked character", async () => {
    show(paras(["abcdeghk", "mnopqsuv"]));
    await keys();
    p.onFocus(true);
    const click = (x: number, y: number) => {
      const ev = new MouseEvent("click", { bubbles: true, clientX: x });
      Object.defineProperty(ev, "pageY", { value: y });
      root.querySelectorAll("p")[1].dispatchEvent(ev);
    };
    click(20, 40);
    expect(bandTop()).toBe(30);
    expect(glyph()).toBe("o");
    // Past the row's end: its last character.
    click(300, 40);
    expect(glyph()).toBe("v");
    // The click's character, not the earlier column, is what `k` keeps.
    p.moveRows(-1);
    expect(glyph()).toBe("k");
  });
});

describe("without kit 6", () => {
  it("has no block, and the rows do nothing", async () => {
    show("hello\n\nworld");
    delete (kitStub as Partial<typeof kitStub>).motion;
    try {
      await keys();
      p.onFocus(true);
      expect(shown()).toBe(false);
      expect(p.cursorBand()).not.toBeNull();
      for (const a of ["cursor.left", "cursor.right", "cursor.word_next", "cursor.word_prev", "cursor.word_end",
        "cursor.line_start", "cursor.line_end"]) expect(press(a)).toBe(true);
      p.moveRows(1);
      expect(bandTop()).toBe(30);
      expect(shown()).toBe(false);
    } finally {
      (kitStub as Record<string, unknown>).motion = (at: Place, m: string) => { motionCalls.push([at, m]); return motion(at, m); };
    }
  });
});

describe("the rows run the moves", () => {
  it("each of the seven by its action name", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    press("cursor.right");
    press("cursor.right");
    expect(p.block.index()).toBe(2);
    press("cursor.left");
    expect(p.block.index()).toBe(1);
    press("cursor.line_end");
    expect(p.block.index()).toBe(10);
    press("cursor.line_start");
    expect(p.block.index()).toBe(0);
    for (const [action, m] of [["cursor.word_next", "w"], ["cursor.word_prev", "b"], ["cursor.word_end", "e"]]) {
      motionCalls.length = 0;
      press(action);
      expect(motionCalls.map((c) => c[1])).toEqual([m]);
    }
  });
});

describe("review fixes", () => {
  it("holding l costs the same at every column of a long row", () => {
    const n = 600;
    show(`\`\`\`\n${"x".repeat(2000)}\n\`\`\``);
    p.onFocus(true);
    const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
    const real = proto.getClientRects;
    let reads = 0;
    Object.defineProperty(proto, "getClientRects", {
      configurable: true, value: function (this: Range) { reads++; return real.call(this); },
    });
    try {
      const per: number[] = [];
      for (let i = 0; i < n; i++) {
        reads = 0;
        expect(p.blockMove("l")).toBe(true);
        per.push(reads);
      }
      expect(p.block.index()).toBe(n);
      expect(Math.max(...per.slice(0, 5))).toBeLessThanOrEqual(12);
      expect(Math.max(...per.slice(-5))).toBeLessThanOrEqual(Math.max(...per.slice(0, 5)) + 2);
    } finally {
      Object.defineProperty(proto, "getClientRects", { configurable: true, value: real });
    }
  });

  it("a 0 that has nowhere to go still ends the end-of-row column", () => {
    show(paras(["abcdeghk", "m", "opqsuvwxyz"]));
    p.onFocus(true);
    p.blockMove("$");
    p.moveRows(1);
    expect(glyph()).toBe("m");
    expect(p.blockMove("0")).toBe(false);
    p.moveRows(1);
    expect(glyph()).toBe("o");
  });
});

/* Visual mode from the block: the kit's `visual.start` and `visual.follow` hooks, with a selection set as the kit sets
   it (the anchor before its character, the focus after the cursor's character going forward, before it going back;
   linewise, at the ends of the lines). A kit 6 build gives `follow` its cursor as a place too; an older one gives the
   focus character's box alone. */
describe("visual mode from the block", () => {
  const visual = () => installed!.visual!;
  const textOf = (text: string) => placeIn(text).node as Text;
  const mode = (kind: "char" | "line" | null) => {
    if (kind) document.documentElement.setAttribute("data-eitri-visual", kind);
    else document.documentElement.removeAttribute("data-eitri-visual");
  };
  const select = (a: Place, f: Place) => document.getSelection()!.setBaseAndExtent(a.node, a.offset, f.node, f.offset);
  /** What the kit does after moving visual mode's cursor: `follow` with the box of the selection's focus character
   *  (the one before the focus going forward, the one after it going back) and, from a kit that has it, `cursor`. */
  const follow = (cursor?: Place) => {
    const s = document.getSelection()!;
    const focus = s.focusNode as Text;
    const forward = s.anchorNode!.compareDocumentPosition(focus) & Node.DOCUMENT_POSITION_FOLLOWING ||
      (s.anchorNode === focus && s.focusOffset >= s.anchorOffset);
    const i = forward ? Math.max(0, s.focusOffset - 1) : Math.min(s.focusOffset, focus.data.length - 1);
    const r = document.createRange();
    r.setStart(focus, i);
    r.setEnd(focus, i + 1);
    visual().follow!(r.getBoundingClientRect(), cursor);
  };
  afterEach(() => { mode(null); document.getSelection()!.removeAllRanges(); });

  it("v starts at the block's character", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    l(4);
    const at = visual().start!()!;
    expect([at.startContainer, at.startOffset]).toEqual([textOf("hello"), 4]);
    // The same place when the block sits on a later row.
    p.moveRows(1);
    const next = visual().start!()!;
    expect(next.startContainer).toBe(textOf("next"));
    expect(next.startOffset).toBe(3);
  });

  it("starts as before without a block: a row with no text, and a kit before 6", async () => {
    show("hello\n\n---\n\nworld");
    await keys();
    p.onFocus(true);
    p.moveRows(1);
    expect(shown()).toBe(false);
    expect(visual().start!()!.startContainer).toBe(textOf("world"));
    (window as { eitriKeys?: unknown }).eitriKeys = { ...kitStub, version: 5, motion: undefined };
    p.moveRows(-1);
    l(2);
    expect(visual().start!()!.startContainer).toBe(textOf("hello"));
    expect(visual().start!()!.startOffset).toBe(0);
  });

  it("V stays linewise: the line band, not character boxes, with the block's character as the anchor", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    l(4);
    const at = visual().start!()!;
    expect(at.startOffset).toBe(4);
    const hello = textOf("hello");
    mode("line");
    select({ node: hello, offset: 0 }, { node: hello, offset: hello.data.length });
    follow();
    expect(document.querySelector<HTMLElement>(".mdp-visual-band")!.hidden).toBe(false);
    expect(document.querySelectorAll(".mdp-visual-box").length).toBe(0);
    expect(shown()).toBe(true);
  });

  it("V from a kit without its cursor keeps the block on its character, not the line's end, and j takes it down at its column", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    l(4);
    mode("line");
    const hello = textOf("hello");
    // The kit draws the whole line, the focus at its end; its cursor is still on the `o`.
    select({ node: hello, offset: 0 }, { node: hello, offset: hello.data.length });
    follow();
    expect(glyph()).toBe("o");
    // Going back, the focus is at the line's start.
    select({ node: hello, offset: hello.data.length }, { node: hello, offset: 0 });
    follow();
    expect(glyph()).toBe("o");
    // j: both lines drawn, the focus at the end of the next; the block goes there at its column (x 32 to 40), past
    // the end of `next` (32 px), so on its last character.
    const next = textOf("next");
    select({ node: hello, offset: 0 }, { node: next, offset: 4 });
    follow();
    expect(glyph()).toBe("t");
    expect(bandTop()).toBe(30);
    mode(null);
    document.getSelection()!.removeAllRanges();
    p.drawBlock();
    expect(glyph()).toBe("t");
  });

  it("V on a table row whose last cell wraps keeps the block and the band on the cursor's row, past Esc", async () => {
    show(`intro\n\n| h | g |\n| --- | --- |\n| a | TALL ${"wrapping ".repeat(24)}|\n| b | c |`);
    await keys();
    p.onFocus(true);
    const tr = placeIn("a").node.parentElement!.closest("tr")!;
    p.jumpCursorTo(rowTop(tr) + 1);
    expect(glyph()).toBe("a");
    const top = bandTop();
    const start = visual().start!()!;
    const cursor = { node: start.startContainer, offset: start.startOffset };
    // The kit selects the whole row: its focus at the end of the last cell, two wrapped lines below the cursor.
    mode("line");
    const last = placeIn("TALL").node as Text;
    select(cursor, { node: last, offset: last.data.length });
    const end = document.createRange();
    end.setStart(last, last.data.length - 2);
    end.setEnd(last, last.data.length - 1);
    expect(end.getBoundingClientRect().top + window.scrollY).toBeGreaterThanOrEqual(top + 40);
    follow(cursor);
    expect(glyph()).toBe("a");
    expect(bandTop()).toBe(top);
    mode(null);
    document.getSelection()!.removeAllRanges();
    p.drawBlock();
    expect(glyph()).toBe("a");
    expect(bandTop()).toBe(top);
  });

  it("V + l moves the block along the line, the whole line still selected; a line's end is its last character", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    l(4);
    mode("line");
    const hello = textOf("hello");
    select({ node: hello, offset: 0 }, { node: hello, offset: hello.data.length });
    follow({ node: hello, offset: 4 });
    expect(glyph()).toBe("o");
    // l: the kit's cursor moves on, the selection stays the whole line.
    follow({ node: hello, offset: 5 });
    expect(glyph()).toBe(" ");
    expect(p.block.index()).toBe(5);
    // $: the line's end as an element place, after the paragraph's text.
    follow({ node: hello.parentElement!, offset: hello.parentElement!.childNodes.length });
    expect(glyph()).toBe("d");
    expect(bandTop()).toBe(0);
    // Charwise too the block goes to the kit's cursor, not to a character read from the selection.
    mode("char");
    select({ node: hello, offset: 0 }, { node: hello, offset: 3 });
    follow({ node: hello, offset: 1 });
    expect(glyph()).toBe("e");
  });

  it("a scroll in visual mode leaves the cursor and the block on the selection's cursor character", async () => {
    show(paras(Array.from({ length: 60 }, (_, i) => `row${i}`)));
    await keys();
    p.onFocus(true);
    mode("char");
    const row0 = textOf("row0");
    select({ node: row0, offset: 0 }, { node: row0, offset: 2 });
    follow();
    expect(glyph()).toBe("o");
    // <C-e> as far as the first row is above the top margin: the page's cursor is the selection's, and stays.
    layout.setScroll(300);
    vi.advanceTimersByTime(1000);
    expect(bandTop()).toBe(0);
    expect(p.block.caretRange()!.startContainer).toBe(row0);
    expect(glyph()).toBe("o");
    mode(null);
    document.getSelection()!.removeAllRanges();
    p.drawBlock();
    expect(p.block.caretRange()!.startContainer).toBe(row0);
    expect(p.block.caretRange()!.startOffset).toBe(1);
  });

  it("a move along the row after <C-e> scrolled it away brings it back inside the margins", async () => {
    show(paras(Array.from({ length: 60 }, (_, i) => `row${i}`)));
    await keys();
    p.onFocus(true);
    mode("char");
    const row0 = textOf("row0");
    select({ node: row0, offset: 0 }, { node: row0, offset: 2 });
    follow({ node: row0, offset: 1 });
    layout.setScroll(300);
    vi.advanceTimersByTime(1000);
    // l: the kit's cursor moves to the next character of the same row.
    select({ node: row0, offset: 0 }, { node: row0, offset: 3 });
    follow({ node: row0, offset: 2 });
    vi.advanceTimersByTime(1000);
    expect(glyph()).toBe("w");
    expect(bandTop()).toBe(0);
    expect(window.scrollY).toBe(0);
  });

  it("a line's end on a blank code line leaves the band on that line and no block", async () => {
    show("```\nab\n\ncd\n```");
    await keys();
    p.onFocus(true);
    mode("char");
    const code = textOf("ab");
    const box = (i: number) => {
      const r = document.createRange();
      r.setStart(code, i);
      r.setEnd(code, i + 1);
      return r.getBoundingClientRect();
    };
    const ab = box(0).top + window.scrollY;
    const cd = box(4).top + window.scrollY;
    expect(cd - ab).toBe(40);
    select({ node: code, offset: 0 }, { node: code, offset: 3 });
    // The kit's cursor is the blank line's end, before its line break; its rect is that line's.
    visual().follow!(new DOMRect(0, box(4).top - 20, 8, 20), { node: code, offset: 3 });
    expect(shown()).toBe(false);
    expect(bandTop()).toBeGreaterThan(ab + 10);
    expect(bandTop()).toBeLessThan(cd - 10);
    // The line's end before the first break is `b`'s.
    visual().follow!(box(1), { node: code, offset: 2 });
    expect(glyph()).toBe("b");
  });

  it("the block follows the selection's cursor end, forward and backward, and the band follows its row", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    l(1);
    mode("char");
    const hello = textOf("hello");
    // Forward from `e` to `l`: the focus is after the cursor's character.
    select({ node: hello, offset: 1 }, { node: hello, offset: 3 });
    follow();
    expect(glyph()).toBe("l");
    expect(shown()).toBe(true);
    expect(document.documentElement.getAttribute("data-eitri-visual")).toBe("char");
    // Backward: the focus is before the cursor's character.
    select({ node: hello, offset: 4 }, { node: hello, offset: 2 });
    follow();
    expect(glyph()).toBe("l");
    expect(p.block.index()).toBe(2);
    select({ node: hello, offset: 4 }, { node: hello, offset: 0 });
    follow();
    expect(glyph()).toBe("h");
    // Another row: the cursor's band goes there with the block.
    const next = textOf("next");
    select({ node: hello, offset: 0 }, { node: next, offset: 3 });
    follow();
    expect(glyph()).toBe("x");
    expect(bandTop()).toBe(30);
    expect(shown()).toBe(true);
    // The focus at a text node's end is the node's last character.
    select({ node: hello, offset: 0 }, { node: hello, offset: hello.data.length });
    follow();
    expect(glyph()).toBe("d");
    expect(bandTop()).toBe(0);
  });

  it("stays on the cursor's character after Esc, and moves on from there", async () => {
    show("hello world\n\nnext");
    await keys();
    p.onFocus(true);
    mode("char");
    const hello = textOf("hello");
    select({ node: hello, offset: 0 }, { node: hello, offset: 5 });
    follow();
    expect(glyph()).toBe("o");
    mode(null);
    document.getSelection()!.removeAllRanges();
    p.placeVisual();
    p.drawBlock();
    expect(shown()).toBe(true);
    expect(glyph()).toBe("o");
    expect(p.block.index()).toBe(4);
    expect(p.blockMove("l")).toBe(true);
    expect(glyph()).toBe(" ");
  });
});
