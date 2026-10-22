import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { copyText, yankCurrentBlock } from "./clipboard";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { Preview } from "./preview";

const FENCE = "`".repeat(3);
let layout: FakeLayout;
let preview: Preview;
let written: string[];
const kit = { version: 2, install: vi.fn(), setCurrent: vi.fn(), message: vi.fn(), status: vi.fn(), keys: vi.fn(() => []) };

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  layout = installFakeLayout(document.getElementById("mdp-root")!);
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  kit.message.mockClear();
  written = [];
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async (t: string) => { written.push(t); } } });
  const bridge: Bridge = { call: async () => ({ applied: true }), on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  preview = new Preview(document.getElementById("mdp-root")!, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => { preview.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks(); });

function show(body: string): void {
  const v: EditorView = { buf: 1, name: "/p/a.md", filetype: "markdown", tick: 1, current: true, line_count: 40, top: 1,
    top_fraction: 0, bottom: 20, cursor: { line: 1, col: 0 }, cause: "user", doc: null };
  preview.onView(v);
  preview.onText({ buf: 1, tick: 1, name: "/p/a.md", text: body });
  vi.advanceTimersByTime(0);
}

describe("copyText", () => {
  it("copies with execCommand in the key event, and uses the clipboard API only when that fails", async () => {
    const doc = document as { execCommand?: unknown };
    const saved = doc.execCommand;
    try {
      const focused = document.createElement("button");
      document.body.append(focused);
      focused.focus();
      const exec = vi.fn(() => true);
      doc.execCommand = exec;
      expect(await copyText("a")).toBe(true);
      expect(exec).toHaveBeenCalledWith("copy");
      expect(written).toEqual([]);
      expect(document.activeElement).toBe(focused);
      exec.mockReturnValue(false);
      const pending = copyText("b");
      expect(written).toEqual(["b"]); // called before the first await, inside the key event
      expect(await pending).toBe(true);
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async () => { throw new Error("no"); } } });
      expect(await copyText("c")).toBe(false);
      doc.execCommand = undefined;
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: undefined });
      expect(await copyText("d")).toBe(false);
      expect(document.querySelector("textarea")).toBeNull();
      focused.remove();
    } finally {
      doc.execCommand = saved;
    }
  });
});

describe("yy", () => {
  it("copies the current paragraph's source lines and a fenced block's code without its fences", async () => {
    show(`para line one\npara line two\n\n${FENCE}sh\nmake\nmake install\n${FENCE}`);
    await yankCurrentBlock(preview);
    expect(written.at(-1)).toBe("para line one\npara line two");
    expect(kit.message).toHaveBeenLastCalledWith("yanked 2 lines");
    layout.setScroll(40);
    await yankCurrentBlock(preview);
    expect(written.at(-1)).toBe("make\nmake install");
    const lines = (console.info as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(lines).toContain('[page-trace] yanked {"lines":2}');
  });
});
