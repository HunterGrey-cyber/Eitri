import { afterEach, expect, it, vi } from "vitest";
import type { Bridge } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { yankCurrentBlock } from "./clipboard";
import { boxAt, currentBlock, lineAt } from "./current";
import { Preview } from "./preview";
import { createEngine } from "../render/engine";
import { sanitizeDocument } from "../render/sanitize";
import { measure } from "./lineindex";

function rect(top: number, height: number): DOMRect {
  return { top, bottom: top + height, height, left: 0, right: 600, width: 600, x: 0, y: top,
    toJSON: () => ({}) } as DOMRect;
}

afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("pairs raw HTML boundaries by ordinal even when the DOM nests their starts", () => {
  const root = document.createElement("main");
  root.innerHTML = '<div class="code-line" data-line="0" data-eitri-html="start:0"></div>' +
    '<div>first<div class="code-line" data-line="10" data-eitri-html="start:1"></div>' +
    'second<span data-eitri-html="end:0"></span></div><span data-eitri-html="end:1"></span>';
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue(rect(0, 0));
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: function (this: Range) {
    return this.toString() === "firstsecond" ? [rect(40, 50)] : this.toString() === "second" ? [rect(60, 30)] : [];
  } });
  try {
    expect(measure(root, 0).map(m => [m.line, m.top, m.height])).toEqual([[0, 40, 50], [10, 60, 30]]);
  } finally {
    delete (Range.prototype as unknown as { getClientRects?: unknown }).getClientRects;
  }
});

it("renders distinct nonce-checked ordinals for raw HTML blocks", () => {
  const env = { nonce: "test", doc: null, remoteImages: "off" as const, blocked: [], counts: {} };
  const rendered = createEngine().render('<div>first</div>\n\n<div data-eitri-html="end:0">second</div>', env);
  const fragment = sanitizeDocument(rendered, env);
  expect([...fragment.querySelectorAll("[data-eitri-html]")].map(el => el.getAttribute("data-eitri-html")))
    .toEqual(["start:0", "end:0", "start:1", "end:1"]);
});

it.each([
  '<div id="raw" class="code-line" data-line="999" data-line-end="1000" data-eitri-html="end">\n<strong>raw content</strong>\n</div>',
  '<table id="raw">\n<tr><td>raw content</td></tr>\n</table>',
  '<div id="raw">raw content</div>\n<div>second node</div>\nplain text',
])("maps raw HTML by its rendered extent and yanks only the renderer's source range: %s", async (html) => {
  vi.useFakeTimers();
  vi.spyOn(console, "info").mockImplementation(() => {});
  vi.stubGlobal("requestAnimationFrame", undefined);
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root"></main>';
  const root = document.getElementById("mdp-root")!;
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this.id === "user-content-raw" || this.closest("#user-content-raw")) return rect(40, 50);
    if (this.tagName === "P" && this.textContent === "next block") return rect(120, 20);
    return rect(0, 0);
  });
  // jsdom has no range layout. Only the raw HTML's nodes have these two visible rows; the empty markers have none.
  vi.stubGlobal("scrollY", 0);
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: function (this: Range) {
    return this.toString().includes("raw content") ? [rect(40, 20), rect(60, 30)] : [];
  } });
  const copy = vi.fn(async () => {});
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: copy } });
  const bridge: Bridge = { call: async () => ({ applied: true }), on: () => () => {} };
  const runner: MermaidRunner = { fill: () => 0, renderPending: () => {}, setTheme: () => {} };
  const p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "off" }, runner);
  try {
    p.onText({ buf: 1, tick: 1, name: "/p/1.md", text: `${html}\n\nnext block` });
    vi.advanceTimersByTime(0);
    const raw = root.querySelector("#user-content-raw")!;
    expect(raw.hasAttribute("data-line")).toBe(false);
    expect(raw.hasAttribute("data-eitri-html")).toBe(false);
    const index = p.index();
    const box = boxAt(index, 65)!;
    expect([box.line, box.end, box.top, box.height]).toEqual([0, 3, 40, 50]);
    expect(lineAt(index, 65)).toBe(1);
    expect(p.sourceLineAt(65)).toBe(1);
    p.onFocus(true);
    p.cursorToPoint(65);
    expect(currentBlock(p)).toBe(box);
    await yankCurrentBlock(p);
    expect(copy).toHaveBeenLastCalledWith(html);
  } finally {
    p.dispose();
    delete (Range.prototype as unknown as { getClientRects?: unknown }).getClientRects;
    delete (navigator as unknown as { clipboard?: unknown }).clipboard;
  }
});
