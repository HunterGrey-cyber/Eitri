import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { currentBlock } from "./current";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { installKeys } from "./keys";
import { Preview } from "./preview";
import { HELD_REVEAL_MAX_MS, HELD_REVEAL_QUIET_MS } from "./sync";

/* The fake layout: paragraph p<i> (line 2i) at 30i, 20 px high, one row; a TALL paragraph wraps onto six rows; a
   HUGE image is 1020 px. The page's text is 20 px a line, so the reserve (the margins) is 100 px and the space
   between them in the 768 px view is 568 px. */
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const paragraphs = (n: number) => Array.from({ length: n }, (_, i) => `p${i}`).join("\n\n");
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 200, top: 1,
    top_fraction: 0, bottom: 40, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}

let layout: FakeLayout;
let p: Preview;
let calls: { verb: string; args: unknown }[];
const reveals = () => calls.filter((c) => c.verb === "editor.reveal").map((c) => c.args as Record<string, number>);
const top = () => p.cursorBand()!.top;
const press = (fn: () => void, times = 1) => { for (let i = 0; i < times; i++) fn(); };

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  const root = document.getElementById("mdp-root")!;
  layout = installFakeLayout(root);
  root.style.lineHeight = "20px";
  calls = [];
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied: true }; }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => { p.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks(); });

function show(body: string, v: Partial<EditorView> = {}): void {
  p.onView(view(v));
  p.onText({ buf: 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}

describe("the cursor's rows", () => {
  it("j walks the cursor down while the page stays, then the page moves a row per j at the bottom margin", () => {
    show(paragraphs(60));
    p.onFocus(true);
    expect([top(), window.scrollY]).toEqual([0, 0]);
    press(() => p.moveRows(1), 21);
    // p21 ends at 650, inside the bottom margin (768 - 100).
    expect([top(), window.scrollY]).toEqual([630, 0]);
    p.moveRows(1);
    expect([top(), window.scrollY]).toEqual([660, 12]);
    p.moveRows(1);
    expect([top(), window.scrollY]).toEqual([690, 42]);
    // k comes back up without scrolling until the top margin, which gives way at the document's start.
    press(() => p.moveRows(-1), 18);
    expect([top(), window.scrollY]).toEqual([150, 42]);
    p.moveRows(-1);
    expect([top(), window.scrollY]).toEqual([120, 20]);
    press(() => p.moveRows(-1), 4);
    expect([top(), window.scrollY]).toEqual([0, 0]);
    p.moveRows(-1);
    expect([top(), window.scrollY]).toEqual([0, 0]);
  });

  it("goes through a wrapped paragraph row by row, telling nvim nothing while the source line stays", () => {
    show("p0\n\nTALL p1\n\np2", { rows: 48, cursor_row: 1 });
    p.onFocus(true);
    p.moveRows(1);
    vi.advanceTimersByTime(100);
    expect(reveals()).toEqual([{ buf: 1, line: 1, token: 1, cursor: 3, row: 3 }]);
    for (let i = 1; i < 6; i++) {
      p.moveRows(1);
      vi.advanceTimersByTime(100);
      expect(top()).toBe(30 + 20 * i);
    }
    expect(reveals().length).toBe(1);
    p.moveRows(1);
    vi.advanceTimersByTime(100);
    expect(top()).toBe(160);
    expect(reveals().at(-1)).toMatchObject({ cursor: 5 });
  });

  it("goes through a block with no text taller than the space between the margins a line at a time", () => {
    show("p0\n\n![HUGE](x.png)\n\np2");
    p.onFocus(true);
    p.moveRows(1);
    expect(p.cursorBand()).toMatchObject({ top: 30, bottom: 50 });
    p.moveRows(1);
    expect(top()).toBe(50);
    press(() => p.moveRows(1), 30);
    // The band 650..670 crosses the bottom margin at 668 by 2 px.
    expect([top(), window.scrollY]).toEqual([650, 2]);
    press(() => p.moveRows(1), 19);
    expect(p.cursorBand()).toMatchObject({ top: 1030, bottom: 1050 });
    p.moveRows(1);
    expect(top()).toBe(1060);
    p.moveRows(-1);
    expect(top()).toBe(1030);
  });
});

describe("a held j or k", () => {
  /* The kit's glide, as the page sees it: `scrollTo` starts one (here it records the target and moves nothing),
     `settle` ends it. */
  let glides: number[];
  let settle: ReturnType<typeof vi.fn>;
  beforeEach(() => {
    glides = [];
    settle = vi.fn();
    (window as unknown as { eitriKeys: unknown }).eitriKeys = {
      version: 5, scrollTo: (y: number) => { glides.push(y); }, destination: () => window.scrollY, settle,
    };
  });
  afterEach(() => { delete (window as unknown as { eitriKeys?: unknown }).eitriKeys; });
  /** The window position that puts the cursor's band on the bottom margin (768 - 100). */
  const atBottomMargin = () => top() + 20 - 668;
  /** The cursor at the bottom margin, by single presses, with every reveal of them sent. */
  function toBottomMargin(): number {
    show(paragraphs(120));
    p.onFocus(true);
    press(() => p.moveRows(1), 21);
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS);
    return reveals().length;
  }

  it("scrolls the page with each repeat at once, no glide, and tells nvim once, when the repeats stop", () => {
    const sent = toBottomMargin();
    for (let i = 0; i < 20; i++) {
      p.moveRows(1, true);
      expect(window.scrollY).toBe(atBottomMargin());
      vi.advanceTimersByTime(33);
    }
    expect(glides).toEqual([]);
    expect(settle).toHaveBeenCalled();
    expect(reveals().length).toBe(sent);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(reveals().length).toBe(sent + 1);
    // p41 is at 1230: source line 83.
    expect(top()).toBe(1230);
    expect(reveals().at(-1)).toMatchObject({ cursor: 83, line: Math.floor(window.scrollY / 30) * 2 + 1 });
    // The next single press glides again and tells nvim at once.
    p.moveRows(1);
    expect(glides.length).toBe(1);
    expect(reveals().length).toBe(sent + 2);
  });

  it("moves nvim once a second while the key stays down", () => {
    const sent = toBottomMargin();
    for (let i = 0; i < 40; i++) { p.moveRows(1, true); vi.advanceTimersByTime(33); }
    expect(reveals().length).toBe(sent + 1);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(reveals().length).toBe(sent + 2);
    expect(reveals().at(-1)).toMatchObject({ cursor: 2 * (21 + 40) + 1 });
  });

  it("ends the hold when the repeats stop just after a once-a-second reveal: a wheel scroll then reveals at once", () => {
    const sent = toBottomMargin();
    // The last repeat 990 ms after the first, then nothing: the max interval's reveal comes first, then the stop.
    for (let i = 0; i < 31; i++) { p.moveRows(1, true); if (i < 30) vi.advanceTimersByTime(33); }
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS - 990);
    expect(reveals().length).toBe(sent + 1);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    // Nothing moved since: the stop sends nothing more.
    expect(reveals().length).toBe(sent + 1);
    window.dispatchEvent(new Event("wheel"));
    layout.setScroll(window.scrollY + 300);
    expect(reveals().length).toBe(sent + 2);
  });

  it("tells nvim where it stopped at once when the keys leave mid-hold, and nothing after", () => {
    const sent = toBottomMargin();
    for (let i = 0; i < 5; i++) { p.moveRows(1, true); vi.advanceTimersByTime(33); }
    p.onFocus(false);
    expect(reveals().length).toBe(sent + 1);
    expect(reveals().at(-1)).toMatchObject({ cursor: 2 * (21 + 5) + 1 });
    vi.advanceTimersByTime(HELD_REVEAL_MAX_MS * 2);
    expect(reveals().length).toBe(sent + 1);
  });

  it("goes up as it goes down: k held scrolls at once and tells nvim when it stops", () => {
    toBottomMargin();
    for (let i = 0; i < 30; i++) { p.moveRows(1, true); vi.advanceTimersByTime(33); }
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    const sent = reveals().length;
    const y = window.scrollY;
    // Up to the top margin first (the window stays), then a row of scroll per repeat.
    for (let i = 0; i < 30; i++) { p.moveRows(-1, true); vi.advanceTimersByTime(33); }
    expect(window.scrollY).toBeLessThan(y);
    expect(window.scrollY).toBe(top() - 100);
    expect(reveals().length).toBe(sent);
    vi.advanceTimersByTime(HELD_REVEAL_QUIET_MS);
    expect(reveals().length).toBe(sent + 1);
    expect(glides).toEqual([]);
  });
});

describe("a stretch with no text", () => {
  it("is crossed by j and k even when it is taller than the view", () => {
    // HOLLOW p1 (30..1050) has text on its first row only: 1000 px with no row below it, more than the view.
    show("p0\n\nHOLLOW p1\n\np2");
    p.onFocus(true);
    p.moveRows(1);
    expect(top()).toBe(30);
    p.moveRows(1);
    expect(top()).toBe(1060);
    p.moveRows(-1);
    expect(top()).toBe(30);
  });
});

describe("the cursor's source line", () => {
  it("is never a blank line a block's map ends with, so nvim's cursor and Enter land on the block's text", () => {
    // The last item's map runs over the blank line after it (lines 1 and 2 of "- a / - b / (blank) / para").
    show("- a\n- b\n\npara");
    const item = [...document.querySelectorAll("li")].at(-1)!;
    expect([item.getAttribute("data-line"), item.getAttribute("data-line-end")]).toEqual(["1", "3"]);
    p.onFocus(true);
    p.moveRows(1);
    p.moveRows(1);
    // The item's second row (the fake layout gives each line of its map a row) reads line 2, which is blank.
    expect(p.sourceLineAt(top() + 10)).toBe(1);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ cursor: 2 });
  });
});

describe("the cursor's rows and nvim's", () => {
  it("lands the cursor's line on one of nvim's rows whenever it scrolls the page, so nvim can start it level", () => {
    // 48 rows of nvim's over the 768 px view: 16 px a row.
    show(paragraphs(60), { rows: 48, cursor_row: 1 });
    p.onFocus(true);
    press(() => p.moveRows(1), 22);
    // Just enough would be 12 px (p22's bottom at the 668 margin); 20 puts its top, 640 down the view, on row 41.
    expect([top(), window.scrollY]).toEqual([660, 20]);
    for (let i = 0; i < 10; i++) {
      p.moveRows(1);
      expect((top() - window.scrollY) % 16).toBe(0);
      expect(top() + 20 - window.scrollY).toBeLessThanOrEqual(668);
    }
    vi.advanceTimersByTime(100);
    const last = reveals().at(-1)!;
    expect(last.row - 1).toBe((top() - window.scrollY) / 16);
    // Without nvim's rows (an older host) the page moves just enough, as before.
  });
});

describe("a fractional line height", () => {
  it("still takes j through a tall block a line at a time", () => {
    // 16 px text: 16 * 11 / 7 = 25.142857... px a line.
    document.getElementById("mdp-root")!.style.lineHeight = `${(16 * 11) / 7}px`;
    show("p0\n\n![HUGE](x.png)\n\np2");
    p.onFocus(true);
    p.moveRows(1);
    const first = top();
    for (let i = 1; i <= 5; i++) {
      p.moveRows(1);
      expect(top()).toBeCloseTo(first + (i * 16 * 11) / 7, 2);
    }
  });
});

describe("the cursor's pages and jumps", () => {
  it("<C-d> keeps the cursor's height on the screen; at the document's end the cursor moves alone", () => {
    show(paragraphs(60));
    p.onFocus(true);
    press(() => p.moveRows(1), 10);
    p.halfPage(1);
    expect([top(), window.scrollY]).toEqual([690, 384]);
    p.halfPage(1);
    expect([top(), window.scrollY]).toEqual([1080, 768]);
    // G's place (the last block at the view's bottom) is 1790 - 768: the page goes no further.
    p.halfPage(1);
    expect([top(), window.scrollY]).toEqual([1470, 1022]);
    p.halfPage(1);
    expect([top(), window.scrollY]).toEqual([1770, 1022]);
    p.halfPage(-1);
    // Half a view up from 1780 is 1396 (p46), below the bottom margin of the window at 638: pushed in to p42.
    expect([top(), window.scrollY]).toEqual([1260, 638]);
  });

  it("<C-f> leaves the cursor at the top margin of the next page, <C-b> at the bottom margin of the one before", () => {
    show(paragraphs(60));
    p.onFocus(true);
    p.fullPage(1);
    // The view less two lines: 728; the first row at least 100 px below it is p28 at 840.
    expect([top(), window.scrollY]).toEqual([840, 728]);
    p.fullPage(-1);
    expect([top(), window.scrollY]).toEqual([630, 0]);
    // With nowhere to scroll, the cursor goes to the first row.
    p.fullPage(-1);
    expect([top(), window.scrollY]).toEqual([0, 0]);
  });

  it("centres a far jump and moves the least for a near one", () => {
    show(`## A\n\n${paragraphs(60)}\n\n## B\n\n${paragraphs(30)}`);
    p.onFocus(true);
    // B (at 1830) is far below the bottom margin: centred.
    expect(p.jumpCursorTo(1835)).toBe(true);
    expect([top(), window.scrollY]).toEqual([1830, 1840 - 384]);
    // p51 (at 1560) is just inside the top margin of the window at 1456 (1556): no scroll.
    expect(p.jumpCursorTo(1580)).toBe(true);
    expect([top(), window.scrollY]).toEqual([1560, 1456]);
    // p49 (at 1500) is 56 px above it: the least scroll.
    expect(p.jumpCursorTo(1500)).toBe(true);
    expect([top(), window.scrollY]).toEqual([1500, 1400]);
  });
});

describe("the cursor and the pointer", () => {
  it("stays on its row through a pointer scroll unless a margin pushes it, down or up", () => {
    show(paragraphs(60));
    p.onFocus(true);
    press(() => p.moveRows(1), 10);
    layout.setScroll(100);
    expect(top()).toBe(300);
    layout.setScroll(250);
    expect(top()).toBe(360);
    layout.setScroll(600);
    expect(top()).toBe(720);
    layout.setScroll(0);
    // The bottom margin at 668 pushes it up to p21 (630..650).
    expect(top()).toBe(630);
  });

  it("goes to the clicked row without scrolling, and the clicked block is the current one", async () => {
    show(paragraphs(60));
    const remove = await installKeys(p, { run: () => false });
    try {
      p.onFocus(true);
      const ev = new MouseEvent("click", { bubbles: true });
      Object.defineProperty(ev, "pageY", { value: 160 });
      document.querySelectorAll("p")[5].dispatchEvent(ev);
      expect([top(), window.scrollY]).toEqual([150, 0]);
      expect(currentBlock(p)?.line).toBe(10);
    } finally {
      remove();
    }
  });
});

describe("handing the keys over", () => {
  it("starts on nvim's line where the page shows it, moving nothing, and moves nothing when nvim takes them back", () => {
    show(paragraphs(60), { rows: 48, cursor_row: 13, top: 30, cursor: { line: 41, col: 0 } });
    expect(window.scrollY).toBe(600 - 768 / 4);
    p.onFocus(true);
    expect([top(), window.scrollY]).toEqual([600, 408]);
    expect(reveals()).toEqual([]);
    p.moveRows(1);
    // Line 43 (p21, at 630) is 222 px down the view: row 15 of nvim's 48.
    const sent = reveals().at(-1)!;
    expect(sent).toMatchObject({ cursor: 43, row: 15 });
    p.onView(view({ rows: 48, cursor_row: 15, top: 30, cursor: { line: 43, col: 0 }, cause: "reveal", token: sent.token }));
    p.onFocus(false);
    expect(reveals().length).toBe(1);
    // nvim's own next view, its cursor where the reveal put it: the page stays within the rounding to nvim's rows.
    p.onView(view({ rows: 48, cursor_row: 15, top: 30, cursor: { line: 43, col: 0 } }));
    expect(Math.abs(window.scrollY - 408)).toBeLessThanOrEqual(768 / 48 / 2);
    const bar = document.querySelector<HTMLElement>(".mdp-cursor-bar")!;
    expect([bar.hidden, bar.style.top]).toEqual([false, "630px"]);
    expect(document.querySelector<HTMLElement>(".mdp-cursor-line")!.hidden).toBe(true);
  });
});

describe("the cursor in a table", () => {
  /* A real table row's text sits inside its cell's padding, under the border between rows: the row's top (where its
     source line starts) can lie nearer the row above's text than its own. Here each cell's text is 8 px high, 12 px
     into its row's 20 px box. */
  let undo: () => void = () => {};
  beforeEach(() => {
    const proto = Range.prototype as Range & { getClientRects(): DOMRect[] };
    const had = Object.getOwnPropertyDescriptor(proto, "getClientRects")!;
    Object.defineProperty(proto, "getClientRects", {
      configurable: true,
      value: function (this: Range) {
        const node = this.startContainer;
        const cell = (node.nodeType === Node.TEXT_NODE ? node.parentElement : node as Element)?.closest("td, th");
        const box = cell ? layout.boxOf(cell) : undefined;
        if (!box) return (had.value as () => DOMRect[]).call(this);
        const t = box.top + 12 - window.scrollY;
        return [{ top: t, height: 8, bottom: t + 8, left: 0, right: 600, width: 600, x: 0, y: t, toJSON: () => ({}) }];
      },
    });
    undo = () => Object.defineProperty(proto, "getClientRects", had);
  });
  afterEach(() => undo());

  it("starts on the row of nvim's line, not the row above whose text is nearer that line's top", () => {
    const TABLE = "intro\n\n| a | b |\n| --- | --- |\n| core | 1 |\n| agent | 2 |\n| shell | 3 |\n";
    show(TABLE, { rows: 48, cursor_row: 7, top: 1, cursor: { line: 7, col: 0 } });
    p.onFocus(true);
    const row = [...document.querySelectorAll("tr")].find((r) => r.textContent?.includes("shell"))!;
    const box = layout.boxOf(row)!;
    const band = p.cursorBand()!;
    expect(band.top).toBeGreaterThanOrEqual(box.top);
    expect(band.bottom).toBeLessThanOrEqual(box.top + box.height + 10);
  });
});
