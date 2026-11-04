import { afterEach, assert, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { editHere, installKeys, jumpTo, PAGE_CONTEXT, PAGE_ROWS } from "./keys";
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
afterEach(() => { preview.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks(); });

function show(body: string, v: Partial<EditorView> = {}): void {
  preview.onView(view(v));
  preview.onText({ buf: v.buf ?? 1, tick: 1, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}

describe("the page's rows", () => {
  it("are the page's table, in the page's context, and installed with hello's keyboard values and the find options", async () => {
    expect(PAGE_ROWS.map((r) => [r.action, r.keys])).toEqual([
      ["heading.next", ["]]"]], ["heading.prev", ["[["]], ["scroll.left", ["zh"]], ["scroll.right", ["zl"]],
      ["block.yank", ["yy"]], ["edit.here", ["<CR>"]],
    ]);
    expect(PAGE_ROWS.every((r) => r.context === PAGE_CONTEXT && r.desc.length > 0)).toBe(true);
    preview.onFocus(true);
    await installKeys(preview, { run: () => false, timeoutlen: 300, prefix: "Ctrl+b", settings: { scroll_step: 40 } });
    expect(installed!.rows).toBe(PAGE_ROWS);
    expect(installed!.initial).toEqual({ timeoutlen: 300, hasKeys: true, prefix: "Ctrl+b", settings: { scroll_step: 40 } });
    expect(installed!.targets).toBeUndefined();
    expect(installed!.find!.exclude).toBe(".katex-mathml, .mdp-diagram svg");
    expect(installed!.find!.root!()).toBe(document.getElementById("mdp-root"));
  });

  it("declare no half-page or search row: the kit's generic rows have those keys", () => {
    const actions = PAGE_ROWS.map((r) => r.action);
    expect(actions.filter((a) => a.startsWith("scroll.half_") || a.startsWith("search.") || a.startsWith("find.")))
      .toEqual([]);
  });

  it("answer mode.escape with false, so the kit moves focus off a focused control", async () => {
    const run = vi.fn(() => true);
    await installKeys(preview, { run });
    expect(press("mode.escape")).toBe(false);
    expect(run).not.toHaveBeenCalled();
  });

  it("hand any other action to the caller", async () => {
    const run = vi.fn(() => true);
    await installKeys(preview, { run });
    expect(press("block.yank")).toBe(true);
    expect(run).toHaveBeenCalledWith("block.yank", expect.any(KeyboardEvent));
  });
});

describe("the kit's find", () => {
  it("is refreshed after a render, not after a view, a scroll or a focus change", async () => {
    await installKeys(preview, { run: () => false });
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
    await installKeys(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.wrapBottom"]).toBe("search hit BOTTOM, continuing at TOP");
  });

  it("says vim's top wrap text", async () => {
    await installKeys(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.wrapTop"]).toBe("search hit TOP, continuing at BOTTOM");
  });

  it("shows the position as [i/n]", async () => {
    await installKeys(preview, { run: () => false });
    expect(installed!.labels!.messages!["find.count"]).toBe("[{index}/{total}]");
  });
});

describe("headings and sideways", () => {
  it("put the next and previous heading at the reading reserve below the top", async () => {
    show("# A\n\np1\n\n## B\n\np2\n\n## C\n\np3");
    await installKeys(preview, { run: () => false });
    const tops = [...document.querySelectorAll("h1, h2")].map((h) => layout.boxOf(h)!.top);
    press("heading.next");
    expect(window.scrollY).toBe(tops[1] - 44);
    press("heading.next");
    expect(window.scrollY).toBe(tops[2] - 44);
    press("heading.prev");
    expect(window.scrollY).toBe(tops[1] - 44);
  });

  it("glide through the kit and step from where its glide is heading, so quick presses add up", async () => {
    const body = Array.from({ length: 8 }, (_, i) => `## H${i}\n\np${i}`).join("\n\n");
    show(body);
    let heading: number | null = null;
    const scrollTo = vi.fn((y: number) => { heading = y; });
    Object.assign(kit, { version: 4, scrollTo, destination: () => heading ?? window.scrollY });
    try {
      await installKeys(preview, { run: () => false });
      const tops = [...document.querySelectorAll("h2")].map((h) => layout.boxOf(h)!.top);
      press("heading.next");
      press("heading.next");
      press("heading.next");
      expect(scrollTo.mock.calls.map((c) => c[0])).toEqual([tops[1] - 44, tops[2] - 44, tops[3] - 44]);
      expect(window.scrollY).toBe(0);
      press("heading.prev");
      expect(scrollTo.mock.calls.at(-1)![0]).toBe(tops[2] - 44);
    } finally {
      Object.assign(kit, { version: 3, scrollTo: undefined, destination: undefined });
    }
  });

  it("leave the line left and the heading reached in the history", async () => {
    show("# A\n\np1\n\n## B\n\np2\n\n## C\n\np3");
    await installKeys(preview, { run: () => false });
    const replace = vi.spyOn(history, "replaceState");
    const push = vi.spyOn(history, "pushState");
    press("heading.next");
    expect(replace.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 0 });
    expect(push.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1 });
    expect((push.mock.calls.at(-1)![0] as { line: number }).line).toBeCloseTo(16 / 20 * 1, 5);
  });

  it("scroll the current display formula sideways", async () => {
    show("$$\n\\rule{20em}{1em}\n$$\n\npara");
    await installKeys(preview, { run: () => false });
    const display = document.querySelector(".katex-display")!;
    const scrollBy = vi.fn();
    display.scrollBy = scrollBy as typeof display.scrollBy;
    press("scroll.right");
    press("scroll.left");
    expect(scrollBy.mock.calls.map((c) => (c[0] as ScrollToOptions).left)).toEqual([60, -60]);
  });

  it("scroll the current wide block sideways", async () => {
    show(`${FENCE}sh\n${"x".repeat(400)}\n${FENCE}\n\npara`);
    await installKeys(preview, { run: () => false });
    const pre = document.querySelector("pre")!;
    const scrollBy = vi.fn();
    pre.scrollBy = scrollBy as typeof pre.scrollBy;
    press("scroll.right");
    press("scroll.left");
    expect(scrollBy.mock.calls.map((c) => (c[0] as ScrollToOptions).left)).toEqual([60, -60]);
  });
});

describe("edit here", () => {
  it("moves nvim's cursor to the current block's first line and gives nvim the keys", async () => {
    show("first\n\nsecond\n\nthird");
    preview.onFocus(true);
    // The keys came in with nvim's cursor on line 1, on screen: that block is current and its line kept.
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 1, col: 1, focus: true } });
    // Near the start the reserve is what is scrolled away: at 10 the reading point is at 20, below the first block.
    layout.setScroll(10);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 3, col: 0, focus: true } });
    layout.setScroll(30);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 5, col: 0, focus: true } });
  });

  it("keeps nvim's cursor on a blank line between paragraphs when the keys come in and nothing scrolls", async () => {
    // p<k> is on line 2k (zero-based), so the blank line 27 lies between p13 and p14.
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"), { top: 21, cursor: { line: 28, col: 0 } });
    expect(window.scrollY).toBe(300);
    await installKeys(preview, { run: () => false });
    preview.onFocus(true);
    // The bar marks the nearest block (the paragraph above, on a tie); the jump keeps nvim's own line.
    expect(document.querySelector<HTMLElement>(".mdp-current-bar")!.style.top).toBe(`${13 * 30}px`);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 28, col: 1, focus: true } });
  });

  it("takes the nearer block for a cursor line in a wider gap, and the reading line once the reader scrolls", async () => {
    // Raw blank-separated paragraphs with three blank lines between p1 (line 2) and p2 (line 6).
    show("p0\n\np1\n\n\n\np2\n\np3", { cursor: { line: 6, col: 0 } });
    await installKeys(preview, { run: () => false });
    preview.onFocus(true);
    // Line 5 (zero-based) is one line above p2 and three below p1's last line.
    expect(document.querySelector<HTMLElement>(".mdp-current-bar")!.style.top).toBe("60px");
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 6, col: 1, focus: true } });
    preview.onFocus(true);
    layout.setScroll(10);
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
    layout.setScroll(10);
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
    expect(line).toBe(Math.floor((200 + 44) / 20) + 1);
  });

  it("keeps the line nvim put its cursor on for the reading line, so the jump scrolls nothing", async () => {
    show(`${FENCE}\n${Array.from({ length: 40 }, (_, i) => `l${i}`).join("\n")}\n${FENCE}`);
    preview.onFocus(true);
    layout.setScroll(200);
    vi.advanceTimersByTime(100);
    const sent = calls.filter((c) => c.verb === "editor.reveal").at(-1)!.args as { token: number; cursor: number };
    expect(sent.cursor).toBe(13);
    // nvim held the cursor two lines lower, inside its 'scrolloff'.
    preview.onView(view({ top: 11, cursor: { line: 15, col: 0 }, cause: "reveal", token: sent.token }));
    await editHere(preview);
    // The reveal put the cursor at the line's first byte: Enter lands on its first non-blank character.
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 15, col: 0, focus: true } });
    // A cursor nvim kept further along the held line keeps its column.
    preview.onView(view({ top: 11, cursor: { line: 15, col: 1 }, cause: "reveal", token: sent.token }));
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 15, col: 2, focus: true } });
    // Another reading line: its own line again until nvim answers for it.
    layout.setScroll(400);
    await editHere(preview);
    expect((calls.at(-1)!.args as { line: number }).line).toBe(Math.floor((400 + 44) / 20) + 1);
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
      // The block at the glide's destination's reading point.
      expect(line).toBe(Math.floor((300 + 44 - 60) / 30) * 2 + 4 + 1);
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
    await installKeys(preview, { run: () => false });
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
    await installKeys(preview, { run: () => false });
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
    show(Array.from({ length: 20 }, (_, i) => `p${i}`).join("\n\n"));
    await installKeys(preview, { run: () => false });
    const target = document.querySelectorAll("p")[15];
    jumpTo(preview, target);
    expect(window.scrollY).toBe(450 - 44);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 1, line: 0 } }));
    expect(window.scrollY).toBe(0);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 2, line: 30 } }));
    expect(window.scrollY).toBe(0);
  });

  it("records the kit's own jumps (gg, G, a search's move) with the line left and the line reached", async () => {
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"));
    await installKeys(preview, { run: () => false });
    const replace = vi.spyOn(history, "replaceState");
    const push = vi.spyOn(history, "pushState");
    layout.setScroll(300);
    installed!.onJump!("before");
    window.scrollTo({ top: 600 });
    installed!.onJump!("after");
    expect(replace.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 20 });
    expect(push.mock.calls.at(-1)![0]).toMatchObject({ mdp: true, buf: 1, line: 40 });
    const pushes = push.mock.calls.length;
    installed!.onJump!("before");
    installed!.onJump!("after");
    expect(push.mock.calls.length).toBe(pushes);
  });

  it("gives the kit G's place: the last block at the bottom of the view", async () => {
    show(Array.from({ length: 40 }, (_, i) => `p${i}`).join("\n\n"));
    await installKeys(preview, { run: () => false });
    expect(installed!.bottom!()).toBe(39 * 30 + 20 - window.innerHeight);
  });

  it("leaves no scroll restoration to the browser while installed, and puts its setting back after", async () => {
    history.scrollRestoration = "auto";
    const remove = await installKeys(preview, { run: () => false });
    expect(history.scrollRestoration).toBe("manual");
    remove();
    expect(history.scrollRestoration).toBe("auto");
  });
});
