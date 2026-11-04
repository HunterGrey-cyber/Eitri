import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { yankCurrentBlock } from "./clipboard";
import { currentBlock, noteClick } from "./current";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { editHere, installKeys } from "./keys";
import { Preview } from "./preview";

/* The fake layout (fakelayout.ts): paragraph p<i> (line 2i) at 30i, 20 px a row. The page's text is 20 px a line
   unless a test says otherwise, so the reserve (the margins) is 100 px in the 768 px view. */
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const FENCE = "`".repeat(3);
const paragraphs = (n: number) => Array.from({ length: n }, (_, i) => `p${i}`).join("\n\n");
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 2000, top: 1,
    top_fraction: 0, bottom: 40, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}

let layout: FakeLayout;
let p: Preview;
let root: HTMLElement;
let calls: { verb: string; args: unknown }[];
let removers: (() => void)[] = [];
let installed: InstallOptions | null = null;
const kit = { version: 3, install: vi.fn(async (o: InstallOptions) => { installed = o; return () => {}; }), setCurrent: vi.fn(),
  message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []), findRefresh: vi.fn(), findSeed: vi.fn() };
const reveals = () => calls.filter((c) => c.verb === "editor.reveal").map((c) => c.args as Record<string, number>);
const jumps = () => calls.filter((c) => c.verb === "editor.jump").map((c) => c.args as Record<string, number>);
const top = () => p.cursorBand()!.top;
const press = (action: string, times = 1) => {
  for (let i = 0; i < times; i++) installed!.run(action, new KeyboardEvent("keydown"));
};

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root);
  root.style.lineHeight = "20px";
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  installed = null;
  calls = [];
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied: true }; }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => {
  for (const r of removers.splice(0)) r();
  p.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks();
  delete (window as unknown as { eitriKeys?: unknown }).eitriKeys;
});

function show(body: string, v: Partial<EditorView> = {}): void {
  p.onView(view(v));
  p.onText({ buf: 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}
async function keys(): Promise<void> { removers.push(await installKeys(p, { run: () => false })); }

describe("]] and [[ (1)", () => {
  it("step through every heading when the cursor's band reaches past the heading's own box", async () => {
    // A 26 px line gives each 20 px row 3 px of leading above and below: the band starts above the heading's box.
    root.style.lineHeight = "26px";
    show("# A\n\np\n\n## B\n\np\n\n#### C\n\np\n\n## 第二节\n\np");
    await keys();
    p.onFocus(true);
    const heads = [...document.querySelectorAll("h1, h2, h4")].map((h) => layout.boxOf(h)!.top);
    expect(heads.length).toBe(4);
    for (let i = 1; i < 4; i++) {
      press("heading.next");
      expect(top()).toBe(heads[i] - 3);
    }
    press("heading.next");
    expect(top()).toBe(heads[3] - 3);
    for (let i = 2; i >= 0; i--) {
      press("heading.prev");
      expect(top()).toBe(heads[i] - 3);
    }
  });
});

describe("the picked line (2)", () => {
  it("is what a scroll reveals while it holds, so Enter still finds nvim on it", async () => {
    // nvim on line 28, the blank line between p13 (line 26) and p14 (line 28 is zero-based 27).
    show(paragraphs(40), { top: 21, cursor: { line: 28, col: 0 } });
    expect(window.scrollY).toBe(300);
    p.onFocus(true);
    // The page's own scroll to nvim's place is over (its guard lets the reader's next scroll through).
    vi.advanceTimersByTime(200);
    // A scroll that leaves the cursor (p13, at 390) inside the margins and shows nvim another top line: the reveal
    // keeps nvim's cursor on its blank line rather than the cursor's paragraph's.
    layout.setScroll(270);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ line: 19, cursor: 28 });
    await editHere(p);
    expect(jumps().at(-1)).toEqual({ buf: 1, line: 28, col: 1, focus: true });
    // Once the cursor moves the pick is gone, and the cursor's own line goes (after the edit jump's second, in which
    // nothing would move nvim's cursor off the jump's line).
    vi.advanceTimersByTime(1100);
    p.moveRows(1);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ cursor: 29 });
  });
});

describe("a row between blocks (3)", () => {
  // Intro (line 0) at 0; "- Fruits" (line 2) has no index block of its own: its row at 30; apple (line 3) at 60.
  const DOC_FRUITS = "Intro.\n\n- Fruits\n  - apple\n\npara";

  it("reads its own line, for nvim, Enter and yy", async () => {
    show(DOC_FRUITS);
    p.onFocus(true);
    p.moveRows(1);
    expect(top()).toBe(30);
    expect(p.sourceLineAt(40)).toBe(2);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ cursor: 3 });
    await editHere(p);
    expect(jumps().at(-1)).toMatchObject({ line: 3 });
    const exec = vi.fn(() => true);
    (document as { execCommand?: unknown }).execCommand = exec;
    const copied: string[] = [];
    vi.spyOn(HTMLTextAreaElement.prototype, "select").mockImplementation(function (this: HTMLTextAreaElement) {
      copied.push(this.value);
    });
    p.onFocus(true);
    await yankCurrentBlock(p);
    expect(copied.at(-1)).toBe("- Fruits");
    p.moveRows(1);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ cursor: 4 });
  });

  it("is where the cursor starts when nvim's cursor is on that line", () => {
    show(DOC_FRUITS, { cursor: { line: 3, col: 2 } });
    p.onFocus(true);
    expect(top()).toBe(30);
  });
});

describe("a click that brings the keys (4)", () => {
  it("tells nvim where it put the cursor", () => {
    show(paragraphs(30));
    noteClick();
    p.cursorToPoint(155);
    expect(reveals()).toEqual([]);
    p.onFocus(true);
    expect(reveals().at(-1)).toMatchObject({ cursor: 11 });
    expect(currentBlock(p)?.line).toBe(10);
  });
});

describe("<C-f> and <C-b> with nvim's rows (5)", () => {
  it("land the cursor's line on one of nvim's rows without taking it across a margin", () => {
    // 47 rows of 768 / 47 = 16.34 px: the first row inside the top margin after <C-f> (p28, 112 px down the view)
    // is 13.96 px past row 7's start, so rounding down would put it 98 px down, inside the 100 px margin.
    show(paragraphs(80), { rows: 47, cursor_row: 1 });
    p.onFocus(true);
    p.fullPage(1);
    const cell = 768 / 47;
    const at = top() - window.scrollY;
    expect(at).toBeGreaterThanOrEqual(100 - 0.5);
    expect(Math.abs(at / cell - Math.round(at / cell))).toBeLessThan(1e-6);
    p.fullPage(-1);
    const back = top() + 20 - window.scrollY;
    expect(back).toBeLessThanOrEqual(768 - 100 + 0.5);
  });
});

describe("a fenced code block (6)", () => {
  it("has a row per code line, each the line nvim's cursor goes to", () => {
    // p0 (line 0); the fence opens on line 2, its code is lines 3-5, it closes on line 6; p1 is line 8.
    show(`p0\n\n${FENCE}\na\nb\nc\n${FENCE}\n\np1`);
    p.onFocus(true);
    const got: number[] = [];
    for (let i = 0; i < 4; i++) {
      p.moveRows(1);
      vi.advanceTimersByTime(100);
      got.push(reveals().at(-1)!.cursor);
    }
    expect(got).toEqual([4, 5, 6, 9]);
  });
});

describe("the cost of a move (7)", () => {
  it("reads a long code block's lines once per layout, not at every j", () => {
    const code = Array.from({ length: 500 }, (_, i) => `let x${i} = ${i}; // line ${i}`).join("\n");
    show(`p0\n\n${FENCE}rust\n${code}\n${FENCE}\n\np1`);
    const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
    const real = proto.getClientRects;
    let count = 0;
    Object.defineProperty(proto, "getClientRects", {
      configurable: true, value: function (this: Range) { count++; return real.call(this); },
    });
    try {
      // The keys arriving place the cursor, which reads the rows around it: the whole code block, once.
      p.onFocus(true);
      p.moveRows(1);
      const first = count;
      const per: number[] = [];
      for (let i = 0; i < 20; i++) {
        count = 0;
        p.moveRows(1);
        per.push(count);
      }
      console.log(`getClientRects: keys in and first j ${first}, then ${per.join(",")}`);
      // Highlighting splits each line into several text nodes; after the first read none is read again.
      expect(first).toBeGreaterThan(500);
      expect(Math.max(...per)).toBeLessThanOrEqual(4);
    } finally {
      Object.defineProperty(proto, "getClientRects", { configurable: true, value: real });
    }
  });
});

describe("blank lines inside a block (8)", () => {
  it("are kept: only the blank lines a block's map ends with are not its line", () => {
    show(`p0\n\n${FENCE}\na\n\nb\n${FENCE}\n\np1`);
    p.onFocus(true);
    p.moveRows(1);
    p.moveRows(1);
    vi.advanceTimersByTime(100);
    // The code's second line (line 4, zero-based) is blank, and it is the cursor's.
    expect(reveals().at(-1)).toMatchObject({ cursor: 5 });
  });
});

describe("rows that read as one source line (9)", () => {
  it("keep the cursor on its own row across a re-layout", () => {
    // The footnotes are defined before "end", the last block the index holds, and drawn after it: every row of
    // theirs reads as the line after "end", so only where the cursor was on that line tells them apart.
    show("text[^1] and[^2]\n\n[^1]: note one\n\n[^2]: note two\n\nend");
    p.onFocus(true);
    const tops: number[] = [top()];
    for (let i = 0; i < 10; i++) {
      p.moveRows(1);
      if (top() === tops.at(-1)) break;
      tops.push(top());
    }
    // The last two rows are the two footnotes, below "end".
    expect(tops.length).toBeGreaterThanOrEqual(4);
    p.moveRows(-1);
    const footnote = top();
    expect(footnote).toBe(tops.at(-2));
    root.dispatchEvent(new Event("load"));
    expect(top()).toBe(footnote);
  });
});

describe("a scroll asked of the engine (10)", () => {
  it("does not push back the cursor that asked for it while it is on its way", () => {
    show(paragraphs(60));
    p.onFocus(true);
    p.moveRows(1);
    // The engine's smooth scroll: it starts, and the window gets there over several frames.
    const asked: number[] = [];
    window.scrollTo = ((opts: ScrollToOptions) => {
      if (opts.behavior === "smooth") asked.push(opts.top ?? 0);
      else layout.setScroll(opts.top ?? 0);
    }) as typeof window.scrollTo;
    for (let i = 0; i < 21; i++) p.moveRows(1);
    expect(asked.at(-1)).toBe(12);
    const want = top();
    layout.setScroll(5);
    expect(top()).toBe(want);
    layout.setScroll(12);
    expect(top()).toBe(want);
  });
});

describe("text-less stretches and the document's start (11)", () => {
  it("j crosses two blocks with no rows in a row", () => {
    show("p0\n\nEMPTY a\n\nEMPTY b\n\np3");
    p.onFocus(true);
    p.moveRows(1);
    // EMPTY a at 30..1050, EMPTY b at 1060..2080, p3 at 2090.
    expect(top()).toBe(2090);
    p.moveRows(-1);
    expect(top()).toBe(0);
  });

  it("the first j does not scroll when the document's first row is at the view's top", () => {
    // The first block has no rows; the first row (p1, at 1030) is at the top of the window nvim placed.
    show("EMPTY a\n\np1\n\np2\n\np3", { top: 3, cursor: { line: 3, col: 0 } });
    expect(window.scrollY).toBe(1030);
    p.onFocus(true);
    expect(top()).toBe(1030);
    p.moveRows(1);
    expect([top(), window.scrollY]).toEqual([1060, 1030]);
  });
});
