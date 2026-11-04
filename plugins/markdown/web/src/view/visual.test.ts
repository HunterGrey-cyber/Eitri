import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { editSelection, installKeys, yankSelection } from "./keys";
import { Preview } from "./preview";
import { sourceSelection } from "./selection";

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
  message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []), findRefresh: vi.fn(), findSeed: vi.fn() };
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
