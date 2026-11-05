import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { installKeys, PAGE_ROWS } from "./keys";
import { Preview } from "./preview";

/* `}` and `{`: the cursor by blocks, the ones the margin bar marks. The fake layout (fakelayout.ts): each block 20 px
   a source line and a 10 px gap, a TALL paragraph five rows more; the page's text is 20 px a line, so the reserve
   (the margins) is 100 px in the 768 px view. */
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


describe("} and {", () => {
  it("are the page's keys, and step a block at a time, each a jump that nvim follows", async () => {
    expect(PAGE_ROWS).toContainEqual(expect.objectContaining({ action: "block.next", keys: ["}"] }));
    expect(PAGE_ROWS).toContainEqual(expect.objectContaining({ action: "block.prev", keys: ["{"] }));
    show(paragraphs(10));
    await keys();
    p.onFocus(true);
    const pushed = vi.spyOn(history, "pushState");
    press("block.next");
    expect(top()).toBe(30);
    press("block.next", 2);
    expect(top()).toBe(90);
    expect(pushed).toHaveBeenCalledTimes(3);
    vi.advanceTimersByTime(100);
    expect(reveals().at(-1)).toMatchObject({ cursor: 7 });
    press("block.prev");
    expect(top()).toBe(60);
  });

  it("pass over a wrapped paragraph's other rows; { goes to the start of the block the cursor is inside first", async () => {
    show("p0\n\nTALL one\n\np2");
    await keys();
    p.onFocus(true);
    press("block.next");
    expect(top()).toBe(30);
    press("block.next");
    expect(top()).toBe(160);
    press("block.prev");
    expect(top()).toBe(30);
    p.moveRows(1);
    p.moveRows(1);
    p.moveRows(1);
    expect(top()).toBe(90);
    press("block.prev");
    expect(top()).toBe(30);
    press("block.prev");
    expect(top()).toBe(0);
  });

  it("stop on a list item's own row above its nested list, and on each table row, and pass a code block whole", async () => {
    show("Intro.\n\n- Fruits\n  - apple\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n" + FENCE + "\nx\ny\nz\n" + FENCE + "\n\nend");
    await keys();
    p.onFocus(true);
    const stops = [top()];
    for (let i = 0; i < 6; i++) { press("block.next"); stops.push(top()); }
    const row = (text: string) => layout.boxOf([...document.querySelectorAll("tr")].find((r) => r.textContent?.includes(text))!)!.top;
    const end = layout.boxOf([...document.querySelectorAll("p")].find((x) => x.textContent === "end")!)!.top;
    const pre = layout.boxOf(document.querySelector("pre")!)!.top;
    expect(stops).toEqual([0, 30, 60, row("a"), row("1"), pre, end]);
    press("block.next");
    expect(top()).toBe(end);
    const back = [top()];
    for (let i = 0; i < 6; i++) { press("block.prev"); back.push(top()); }
    expect(back).toEqual([...stops].reverse());
  });
});
