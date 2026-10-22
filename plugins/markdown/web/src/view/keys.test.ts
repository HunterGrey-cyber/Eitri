import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
  it("are spec's table, in the page's context, and installed with hello's keyboard values and the find options", async () => {
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
  it("put the next and previous heading at the top", async () => {
    show("# A\n\np1\n\n## B\n\np2\n\n## C\n\np3");
    await installKeys(preview, { run: () => false });
    const tops = [...document.querySelectorAll("h1, h2")].map((h) => layout.boxOf(h)!.top);
    press("heading.next");
    expect(window.scrollY).toBe(tops[1]);
    press("heading.next");
    expect(window.scrollY).toBe(tops[2]);
    press("heading.prev");
    expect(window.scrollY).toBe(tops[1]);
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
    layout.setScroll(20);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 3, col: 0, focus: true } });
    layout.setScroll(30);
    await editHere(preview);
    expect(calls.at(-1)).toEqual({ verb: "editor.jump", args: { buf: 1, line: 5, col: 0, focus: true } });
  });

  it("names the shown buffer while another buffer's view waits for its text", async () => {
    show("first\n\nsecond\n\nthird");
    preview.onFocus(true);
    layout.setScroll(20);
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
    expect(line).toBe(Math.floor((200 + 22) / 20) + 1);
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
    expect(bar.style.top).toBe("30px");
  });
});

describe("history", () => {
  it("records an in-document jump and comes back to it, ignoring another buffer's entries", async () => {
    show(Array.from({ length: 20 }, (_, i) => `p${i}`).join("\n\n"));
    await installKeys(preview, { run: () => false });
    const target = document.querySelectorAll("p")[15];
    jumpTo(preview, target);
    expect(window.scrollY).toBe(450);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 1, line: 0 } }));
    expect(window.scrollY).toBe(0);
    window.dispatchEvent(new PopStateEvent("popstate", { state: { mdp: true, buf: 2, line: 30 } }));
    expect(window.scrollY).toBe(0);
  });

  it("leaves no scroll restoration to the browser while installed, and puts its setting back after", async () => {
    history.scrollRestoration = "auto";
    const remove = await installKeys(preview, { run: () => false });
    expect(history.scrollRestoration).toBe("manual");
    remove();
    expect(history.scrollRestoration).toBe("auto");
  });
});
