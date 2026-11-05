import { afterEach, assert, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { editHere, installKeys, jumpTo, PAGE_CONTEXT, PAGE_ROWS, pageRows } from "./keys";
import { Preview } from "./preview";

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const FENCE = "`".repeat(3);
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 80, top: 1,
    top_fraction: 0, bottom: 20, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}

let layout: FakeLayout;
let preview: Preview;
let installed: InstallOptions | null;
let calls: { verb: string; args: unknown }[];
let applied: boolean;
const kit = { version: 3, install: vi.fn(async (o: InstallOptions) => { installed = o; return () => {}; }), setCurrent: vi.fn(),
  message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []), findRefresh: vi.fn(), findSeed: vi.fn() };
const press = (action: string) => installed!.run(action, new KeyboardEvent("keydown"));

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  layout = installFakeLayout(document.getElementById("mdp-root")!);
  // The reading reserve is at least five lines of the page's text; these short fake documents are written around a
  // 44 px reserve, so their page text is 8.8 px a line (the floor itself: preview.test.ts).
  document.getElementById("mdp-root")!.style.lineHeight = "8.8px";
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  installed = null;
  calls = [];
  applied = true;
  for (const f of [kit.install, kit.message, kit.status, kit.findRefresh]) f.mockClear();
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied }; }, on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  preview = new Preview(document.getElementById("mdp-root")!, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => {
  for (const remove of removers.splice(0)) remove();
  preview.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks();
});
// Every installation is removed after its test, so no test's history or click listeners act on the next one's page.
const removers: (() => void)[] = [];
async function install(...args: Parameters<typeof installKeys>): Promise<() => void> {
  const remove = await installKeys(...args);
  removers.push(remove);
  return remove;
}

function show(body: string, v: Partial<EditorView> = {}): void {
  preview.onView(view(v));
  preview.onText({ buf: v.buf ?? 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}

describe("the page's rows", () => {
  it("are the page's table, in the page's context, and installed with hello's keyboard values and the find options", async () => {
    expect(PAGE_ROWS.map((r) => [r.action, r.keys])).toEqual([
      ["cursor.down", ["j"]], ["cursor.up", ["k"]], ["cursor.half_down", ["d", "<C-d>"]],
      ["cursor.half_up", ["u", "<C-u>"]], ["cursor.page_down", ["<C-f>"]], ["cursor.page_up", ["<C-b>"]],
      ["cursor.first", ["gg"]], ["cursor.last", ["G"]],
      ["block.next", ["}"]], ["block.prev", ["{"]],
      ["heading.next", ["]]"]], ["heading.prev", ["[["]], ["scroll.left", ["zh"]], ["scroll.right", ["zl"]],
      ["block.yank", ["yy"]], ["edit.here", ["<CR>"]],
    ]);
    expect(PAGE_ROWS.every((r) => r.context === PAGE_CONTEXT && r.desc.length > 0)).toBe(true);
    preview.onFocus(true);
    await install(preview, { run: () => false, timeoutlen: 300, prefix: "Ctrl+b", settings: { scroll_step: 40 } });
    // The stock prefix costs the page its <C-b> row, as it costs the kit its own <C-b>.
    expect(installed!.rows).toEqual(PAGE_ROWS.filter((r) => r.action !== "cursor.page_up"));
    expect(installed!.bottom).toBeUndefined();
    expect(installed!.initial).toEqual({ timeoutlen: 300, hasKeys: true, prefix: "Ctrl+b", settings: { scroll_step: 40 } });
    expect(installed!.targets).toBeUndefined();
    expect(installed!.find!.exclude).toBe(".katex-mathml, .mdp-diagram svg");
    expect(installed!.find!.root!()).toBe(document.getElementById("mdp-root"));
  });

  it("declare no search row and leave the kit <C-e>/<C-y>: those scroll the page, the cursor kept by the margins", () => {
    const actions = PAGE_ROWS.map((r) => r.action);
    expect(actions.filter((a) => a.startsWith("scroll.") && a !== "scroll.left" && a !== "scroll.right")).toEqual([]);
    expect(actions.filter((a) => a.startsWith("search.") || a.startsWith("find."))).toEqual([]);
    expect(PAGE_ROWS.flatMap((r) => r.keys).filter((k) => k === "<C-e>" || k === "<C-y>")).toEqual([]);
  });

  it("drop a key equal to the window's prefix, and every Ctrl key when the window names none", () => {
    const keysOf = (rows: typeof PAGE_ROWS) => Object.fromEntries(rows.map((r) => [r.action, r.keys]));
    expect(keysOf(pageRows("Ctrl+a"))).toEqual(keysOf(PAGE_ROWS));
    expect(keysOf(pageRows("Ctrl+d"))["cursor.half_down"]).toEqual(["d"]);
    const none = keysOf(pageRows(null));
    expect([none["cursor.half_down"], none["cursor.half_up"]]).toEqual([["d"], ["u"]]);
    expect([none["cursor.page_down"], none["cursor.page_up"]]).toEqual([undefined, undefined]);
    expect(keysOf(pageRows("Ctrl+Space"))).toEqual(keysOf(PAGE_ROWS));
  });

  it("answer mode.escape with false, so the kit moves focus off a focused control", async () => {
    const run = vi.fn(() => true);
    await install(preview, { run });
    expect(press("mode.escape")).toBe(false);
    expect(run).not.toHaveBeenCalled();
  });

  it("hand any other action to the caller", async () => {
    const run = vi.fn(() => true);
    await install(preview, { run });
    expect(press("block.yank")).toBe(true);
    expect(run).toHaveBeenCalledWith("block.yank", expect.any(KeyboardEvent));
  });
});

describe("the kit's find", () => {
  it("is refreshed after a render, not after a view, a scroll or a focus change", async () => {
    await install(preview, { run: () => false });
    show("first\n\nsecond");
    expect(kit.findRefresh).toHaveBeenCalled();
    kit.findRefresh.mockClear();
    preview.onView(view({ top: 3 }));
    layout.setScroll(20);
    preview.onFocus(true);
    vi.advanceTimersByTime(0);
    expect(kit.findRefresh).not.toHaveBeenCalled();
  });

  it("says vim's bottom wrap text", async () => {
    await install(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.wrapBottom"]).toBe("search hit BOTTOM, continuing at TOP");
  });

  it("says vim's top wrap text", async () => {
    await install(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.wrapTop"]).toBe("search hit TOP, continuing at BOTTOM");
  });

  it("shows the position as [i/n]", async () => {
    await install(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.count"]).toBe("[{index}/{total}]");
  });
});

describe("headings and sideways", () => {
  it("put the cursor on the next and previous heading, scrolling only when it leaves the margins", async () => {
    // Heading i (line 4i) at 60i, its paragraph at 60i + 30.
    show(Array.from({ length: 40 }, (_, i) => `## H${i}\n\np${i}`).join("\n\n"));
    await install(preview, { run: () => false });
    preview.onFocus(true);
    press("heading.next");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([60, 0]);
    press("heading.prev");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([0, 0]);
    // H12 (720..740) is 16 px past the bottom margin (768 - 44): the page moves those 16 px.
    for (let i = 0; i < 12; i++) press("heading.next");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([720, 16]);
  });

  it("glide through the kit from where its glide is heading, so quick half pages add up", async () => {
    show(Array.from({ length: 80 }, (_, i) => `p${i}`).join("\n\n"));
    let heading: number | null = null;
    const scrollTo = vi.fn((y: number) => { heading = y; });
    Object.assign(kit, { version: 4, scrollTo, destination: () => heading ?? window.scrollY });
    try {
      await install(preview, { run: () => false });
      preview.onFocus(true);
      press("cursor.half_down");
      press("cursor.half_down");
      expect(scrollTo.mock.calls.map((c) => c[0])).toEqual([384, 768]);
      expect(window.scrollY).toBe(0);
      // The first half view took the cursor from block 0 to the row at 394 (block 13), inside the top margin of a
      // window at 384, so it was pushed to the first row below 428 (block 15, at 450); the second took it the same
      // half view on, to block 28 at 840.
      expect(preview.cursorBand()!.top).toBe(840);
    } finally {
      Object.assign(kit, { version: 3, scrollTo: undefined, destination: undefined });
    }
  });

  it("leave the place left and the heading reached in the history, with the cursor's place", async () => {
    show("# A\n\np1\n\n## B\n\np2\n\n## C\n\np3");
    await install(preview, { run: () => false });
    preview.onFocus(true);
    const replace = vi.spyOn(history, "replaceState");
    const push = vi.spyOn(history, "pushState");
    press("heading.next");
    // A (line 0) at 0..20 and B (line 4) at 60..80: the middle of each band.
    expect(replace.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 0, cursor: 0.5 });
    expect(push.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 0, cursor: 4.5 });
  });

  it("scroll the current display formula sideways", async () => {
    show("$$\n\\rule{20em}{1em}\n$$\n\npara");
    await install(preview, { run: () => false });
    const display = document.querySelector(".katex-display")!;
    const scrollBy = vi.fn();
    display.scrollBy = scrollBy as typeof display.scrollBy;
    press("scroll.right");
    press("scroll.left");
    expect(scrollBy.mock.calls.map((c) => (c[0] as ScrollToOptions).left)).toEqual([60, -60]);
  });

  it("scroll the current wide block sideways", async () => {
    show(`${FENCE}sh\n${"x".repeat(400)}\n${FENCE}\n\npara`);
    await install(preview, { run: () => false });
    const pre = document.querySelector("pre")!;
    const scrollBy = vi.fn();
    pre.scrollBy = scrollBy as typeof pre.scrollBy;
    press("scroll.right");
    press("scroll.left");
    expect(scrollBy.mock.calls.map((c) => (c[0] as ScrollToOptions).left)).toEqual([60, -60]);
  });
});

describe("edit here", () => {
  it("moves nvim's cursor to the cursor's block's first line and gives nvim the keys", async () => {
    show("first\n\nsecond\n\nthird");
    preview.onFocus(true);
    // The keys came in with nvim's cursor on line 1, on screen: the cursor starts there and nvim's line is kept.
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 1, col: 1, focus: true } });
    preview.onFocus(true);
    preview.moveRows(1);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 3, col: 0, focus: true } });
    preview.moveRows(1);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 5, col: 0, focus: true } });
  });

  it("keeps nvim's cursor on a blank line between paragraphs when the keys come in and nothing scrolls", async () => {
    // p<k> is on line 2k (zero-based), so the blank line 27 lies between p13 and p14.
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"), { top: 21, cursor: { line: 28, col: 0 } });
    expect(window.scrollY).toBe(300);
    await install(preview, { run: () => false });
    preview.onFocus(true);
    // The bar marks the nearest block (the paragraph above, on a tie); the jump keeps nvim's own line.
    expect(document.querySelector<HTMLElement>(".mdp-current-bar")!.style.top).toBe(`${13 * 30}px`);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 28, col: 1, focus: true } });
  });

  it("takes the nearer block for a cursor line in a wider gap, and the cursor's own line once it moves", async () => {
    // Raw blank-separated paragraphs with three blank lines between p1 (line 2) and p2 (line 6).
    show("p0\n\np1\n\n\n\np2\n\np3", { cursor: { line: 6, col: 0 } });
    await install(preview, { run: () => false });
    preview.onFocus(true);
    // Line 5 (zero-based) is one line above p2 and three below p1's last line.
    expect(document.querySelector<HTMLElement>(".mdp-current-bar")!.style.top).toBe("60px");
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 6, col: 1, focus: true } });
    // A scroll that leaves the cursor inside the margins keeps nvim's line.
    layout.setScroll(10);
    await editHere(preview);
    expect((calls.at(-1)!.args as { line: number }).line).toBe(6);
    preview.moveRows(-1);
    await editHere(preview);
    expect((calls.at(-1)!.args as { line: number }).line).toBe(3);
  });

  it("keeps nvim's column on the line its cursor is on, counted in characters", async () => {
    // Byte 10 of "héllo wörld" is its "r", the ninth character: "é" and "ö" take two bytes each.
    assert.equal(new TextEncoder().encode("héllo wö").length, 10);
    show("p0\n\nhéllo wörld\n\np2", { cursor: { line: 3, col: 10 } });
    preview.onFocus(true);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 3, col: 9, focus: true } });
  });

  it("does not keep a cursor line that is off screen as the keys come in", async () => {
    show(Array.from({ length: 80 }, (_, i) => `p${i}`).join("\n\n"), { cursor: { line: 120, col: 0 } });
    preview.onFocus(true);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 1, col: 0, focus: true } });
  });

  it("names the shown buffer while another buffer's view waits for its text", async () => {
    show("first\n\nsecond\n\nthird");
    preview.onFocus(true);
    preview.moveRows(1);
    // nvim switched to buffer 2; its text has not rendered, so the blocks on the page are still buffer 1's.
    preview.onView(view({ buf: 2, name: "/p/other.md" }));
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 3, col: 0, focus: true } });
  });

  it("takes the line at the reading position inside a long block", async () => {
    show(`${FENCE}\n${Array.from({ length: 40 }, (_, i) => `l${i}`).join("\n")}\n${FENCE}`);
    layout.setScroll(200);
    await editHere(preview);
    const { line } = calls.at(-1)!.args as { line: number };
    // The block's rows are its 40 code lines, from line 2 (the opening fence's line is not one of them).
    expect(line).toBe(Math.floor((200 + 44) / 20) + 2);
  });

  it("keeps the line nvim put its cursor on for the reading line, so the jump scrolls nothing", async () => {
    show(`${FENCE}\n${Array.from({ length: 40 }, (_, i) => `l${i}`).join("\n")}\n${FENCE}`);
    preview.onFocus(true);
    layout.setScroll(200);
    vi.advanceTimersByTime(100);
    // The cursor pushed to the first row at least 44 px below the top: the row at 260, the 14th code line, line 15
    // (the opening fence is line 1).
    const sent = calls.filter((c) => c.verb === "editor.reveal").at(-1)!.args as { token: number; cursor: number };
    expect(sent.cursor).toBe(15);
    // nvim held the cursor two lines lower, inside its 'scrolloff'.
    preview.onView(view({ top: 11, cursor: { line: 17, col: 0 }, cause: "reveal", token: sent.token }));
    await editHere(preview);
    // The reveal put the cursor at the line's first byte: Enter lands on its first non-blank character.
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 17, col: 0, focus: true } });
    // A cursor nvim kept further along the held line keeps its column.
    preview.onView(view({ top: 11, cursor: { line: 17, col: 1 }, cause: "reveal", token: sent.token }));
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 17, col: 2, focus: true } });
    // Another cursor line (pushed to the row at 460): its own line again until nvim answers for it.
    layout.setScroll(400);
    await editHere(preview);
    expect((calls.at(-1)!.args as { line: number }).line).toBe(25);
  });

  it("keeps nvim's held line in the next block when the reading line is a wrapped paragraph at nvim's top", async () => {
    // p10 (line 20) is tall: at 310 it straddles the top and holds the reading point, so the reveal's top and its
    // reading line are the same line, which nvim cannot hold its cursor on under 'scrolloff'.
    const body = Array.from({ length: 30 }, (_, i) => (i === 10 ? "TALL p10" : `p${i}`)).join("\n\n");
    show(body, { scrolloff: 4 });
    preview.onFocus(true);
    layout.setScroll(310);
    vi.advanceTimersByTime(100);
    const sent = calls.filter((c) => c.verb === "editor.reveal").at(-1)!.args as { line: number; token: number; cursor: number };
    expect([sent.line, sent.cursor]).toEqual([21, 21]);
    preview.onView(view({ top: 21, cursor: { line: 23, col: 0 }, cause: "reveal", token: sent.token, scrolloff: 4 }));
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 23, col: 0, focus: true } });
  });

  it("ends a running glide first, and the keys leaving after the jump send no reveal", async () => {
    const settle = vi.fn(() => layout.setScroll(300));
    Object.assign(kit, { version: 4, settle, destination: () => 300 });
    try {
      show("first\n\nsecond\n\n" + Array.from({ length: 30 }, (_, i) => `p${i}`).join("\n\n"));
      preview.onFocus(true);
      layout.setScroll(100);
      vi.advanceTimersByTime(100);
      const before = calls.length;
      await editHere(preview);
      expect(settle).toHaveBeenCalled();
      const after = calls.slice(before);
      expect(after.map((c) => c.verb)).toEqual(["editor.reveal", "editor.jump"]);
      expect(after[0].args).not.toHaveProperty("cursor");
      const { line } = after[1].args as { line: number };
      // The cursor, pushed below the top margin where the glide is heading (300 + 44): p10 at 360, line 25.
      expect(line).toBe(25);
      preview.onFocus(false);
      expect(calls.length).toBe(before + 2);
    } finally {
      Object.assign(kit, { version: 3 });
      delete (kit as { settle?: unknown }).settle;
      delete (kit as { destination?: unknown }).destination;
    }
  });

  it("says so in the HUD when no window shows the file", async () => {
    show("para");
    applied = false;
    await editHere(preview);
    expect(kit.message).toHaveBeenCalledWith("the file is not open in a window");
  });

  it("follows a double-click to the line under the pointer", async () => {
    show("first\n\nsecond\n\nthird");
    await install(preview, { run: () => false });
    const ev = new MouseEvent("dblclick", { bubbles: true });
    Object.defineProperty(ev, "pageY", { value: 65 });
    document.querySelectorAll("p")[2].dispatchEvent(ev);
    await vi.waitFor(() =>
      expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 5, col: 0, focus: true } }));
  });
});

describe("the HUD and the current block", () => {
  it("shows the file, [not current] and the images not loaded, and a bar beside the current block while it has the keys", async () => {
    show("![x](http://example.org/x.png)\n\npara");
    await install(preview, { run: () => false });
    expect(kit.status).toHaveBeenLastCalledWith("notes.md · 1 image not loaded");
    preview.onView(view({ current: false }));
    expect(kit.status).toHaveBeenLastCalledWith("notes.md [not current] · 1 image not loaded");
    const bar = document.querySelector<HTMLElement>(".mdp-current-bar")!;
    expect(bar.hidden).toBe(true);
    preview.onFocus(true);
    expect(bar.hidden).toBe(false);
    // nvim's cursor (line 1) is in the first block, which is on screen.
    expect(bar.style.top).toBe("0px");
    layout.setScroll(10);
    expect(bar.style.top).toBe("30px");
  });
});

describe("history", () => {
  it("records an in-document jump and comes back to it, ignoring another buffer's entries", async () => {
    show(Array.from({ length: 60 }, (_, i) => `p${i}`).join("\n\n"));
    await install(preview, { run: () => false });
    // Without the keys there is no cursor: the target lands at the reserve below the top.
    jumpTo(preview, document.querySelectorAll("p")[15]);
    expect(window.scrollY).toBe(450 - 44);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 1, line: 0 } }));
    expect(window.scrollY).toBe(0);
    preview.onFocus(true);
    // A near target takes the cursor without scrolling.
    jumpTo(preview, document.querySelectorAll("p")[15]);
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([450, 0]);
    // A far one (p40 at 1200, 496 px past the bottom margin) is centred.
    jumpTo(preview, document.querySelectorAll("p")[40]);
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([1200, 1210 - 384]);
    // Back: the entry's cursor (p15) goes back as a jump puts it, centred again from this far.
    const back = history.state as { cursor: number };
    expect(back.cursor).toBeCloseTo(80.5, 5);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 1, line: 40, cursor: 30.5 } }));
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([450, 460 - 384]);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 2, line: 30, cursor: 0.5 } }));
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([450, 460 - 384]);
  });

  it("puts the cursor on a search's match, scrolling only when it is outside the margins, and records the jump", async () => {
    show(Array.from({ length: 80 }, (_, i) => `p${i}`).join("\n\n"));
    await install(preview, { run: () => false });
    preview.onFocus(true);
    const replace = vi.spyOn(history, "replaceState");
    const push = vi.spyOn(history, "pushState");
    layout.setScroll(300);
    const select = (i: number) => {
      const range = document.createRange();
      range.selectNodeContents(document.querySelectorAll("p")[i].firstChild!);
      document.getSelection()!.removeAllRanges();
      document.getSelection()!.addRange(range);
    };
    // The kit moves the window toward the match itself; the cursor's jump replaces that move. p30 (900) is inside
    // the margins seen from 300, so the window goes back there.
    installed!.onJump!("before");
    window.scrollTo({ top: 600 });
    select(30);
    installed!.onJump!("after");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([900, 300]);
    expect(replace.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 20, cursor: 24.5 });
    expect(push.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 20, cursor: 60.5 });
    const pushes = push.mock.calls.length;
    installed!.onJump!("before");
    installed!.onJump!("after");
    expect(push.mock.calls.length).toBe(pushes);
    // A match a little past the bottom margin (p36 at 1080..1100, the margin at 1024) scrolls just enough.
    installed!.onJump!("before");
    select(36);
    installed!.onJump!("after");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([1080, 1100 - 724]);
  });

  it("puts G's cursor on the last row with the last block at the bottom of the view, and gg's on the first", async () => {
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"));
    await install(preview, { run: () => false });
    preview.onFocus(true);
    const push = vi.spyOn(history, "pushState");
    press("cursor.last");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([39 * 30, 39 * 30 + 20 - window.innerHeight]);
    expect(push).toHaveBeenCalledTimes(1);
    press("cursor.first");
    expect([preview.cursorBand()!.top, window.scrollY]).toEqual([0, 0]);
    expect(push).toHaveBeenCalledTimes(2);
    // A held key's repeats move nothing more.
    installed!.run("cursor.last", new KeyboardEvent("keydown", { repeat: true }));
    expect(preview.cursorBand()!.top).toBe(0);
  });

  it("leaves no scroll restoration to the browser while installed, and puts its setting back after", async () => {
    history.scrollRestoration = "auto";
    const remove = await install(preview, { run: () => false });
    expect(history.scrollRestoration).toBe("manual");
    remove();
    expect(history.scrollRestoration).toBe("auto");
  });
});
