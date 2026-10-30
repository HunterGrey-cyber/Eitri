import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { Preview } from "./preview";

/* A page handed between two windows: the preview saves its reading line, a new preview in the other window shows it.
   The boxes are laid out the way WebKit lays the page out in two windows of different widths: box edges at fractions
   of a pixel, paragraphs taller in the narrow window (their text wraps onto more rows), and a scroll offset kept in
   whole pixels with its fraction dropped. jsdom lays nothing out, so the boxes are given here. */
let root: HTMLElement;
let narrow = false;
function layOut(): Map<Element, { top: number; height: number }> {
  const pos = new Map<Element, { top: number; height: number }>();
  let y = 0.671875;
  for (const el of root.querySelectorAll<HTMLElement>(".code-line")) {
    if (el.querySelector(".code-line")) continue;
    const line = Number(el.dataset.line);
    const height = el.dataset.lineEnd === undefined ? 1 : (Number(el.dataset.lineEnd) - line) * (narrow ? 66 : 44) - 10.453125;
    pos.set(el, { top: y, height });
    y += height + 24;
  }
  return pos;
}
function setScroll(y: number): void {
  Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });
  window.dispatchEvent(new Event("scroll"));
}

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const BODY = Array.from({ length: 80 }, (_, i) => `p${i}`).join("\n\n");
function view(top: number, fraction = 0): EditorView {
  return { buf: 1, name: "/p/1.md", filetype: "markdown", tick: 1, current: true, line_count: 160, top, top_fraction: fraction,
    bottom: top + 20, cursor: { line: top, col: 0 }, cause: "user", doc: DOC };
}

let runner: MermaidRunner;
let current: Preview | null = null;

/* A new page, as a window builds one: an empty document at the top. */
function page(): Preview {
  current?.dispose();
  document.body.innerHTML = '<div id="mdp-notice" class="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
  const bridge: Bridge = { call: async () => ({ applied: true }), on: () => () => {} };
  current = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  return current;
}
function render(p: Preview): void { p.onText({ buf: 1, tick: 1, name: "/p/1.md", text: BODY }); vi.advanceTimersByTime(0); }

/* What the page hands over (`host.save`): the line split into a whole line and its fraction, as the state holds it. */
function save(p: Preview): { topLine: number; fraction: number } {
  const line = p.readingLine();
  const topLine = Math.floor(line);
  return { topLine, fraction: line - topLine };
}
/* The successor in the other window: its first view targets the saved line, then its first render shows it. */
function arrive(state: { topLine: number; fraction: number }, inNarrow: boolean): Preview {
  narrow = inNarrow;
  const p = page();
  p.onView(view(state.topLine + 1, state.fraction));
  render(p);
  p.showLine(state.topLine + state.fraction);
  return p;
}

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  narrow = false;
  runner = { fill: vi.fn(() => 0), renderPending: vi.fn(), setTheme: vi.fn() };
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    const p = layOut().get(this);
    const top = (p?.top ?? 0) - window.scrollY;
    return { top, height: p?.height ?? 0, bottom: top + (p?.height ?? 0), left: 0, right: 600, width: 600, x: 0, y: top,
      toJSON: () => ({}) } as DOMRect;
  });
  Object.defineProperty(document.documentElement, "scrollHeight", {
    configurable: true, get: () => [...layOut().values()].reduce((m, p) => Math.max(m, p.top + p.height), 0) + window.innerHeight,
  });
  // The browser keeps whole pixels: the fraction of an offset asked for is dropped.
  window.scrollTo = ((opts: ScrollToOptions) => setScroll(Math.trunc(opts.top ?? 0))) as typeof window.scrollTo;
  window.scrollBy = ((opts: ScrollToOptions) => setScroll(Math.trunc(window.scrollY + (opts.top ?? 0)))) as typeof window.scrollBy;
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => { current?.dispose(); current = null; vi.useRealTimers(); vi.restoreAllMocks(); });

describe("the reading position across a move", () => {
  it("comes back at the offset it was saved at, in a window of the same size, wherever the reader scrolled", () => {
    const lost: number[] = [];
    // Every offset across one paragraph and the gap after it.
    for (let y = 2000; y < 2070; y += 2) {
      const p = page();
      p.onView(view(1));
      render(p);
      p.onFocus(true);
      setScroll(y);
      const next = arrive(save(p), false);
      if (window.scrollY !== y) lost.push(y);
      next.dispose();
    }
    expect(lost).toEqual([]);
  });

  it("does not drift over moves between two windows of different widths", () => {
    // nvim's top line is the blank line after a paragraph, so the page's top edge falls a fraction of a pixel below
    // the paragraph's bottom edge, and the offset kept in whole pixels lies inside the paragraph, whose height
    // differs between the windows.
    const p = page();
    p.onView(view(102));
    render(p);
    const wide = window.scrollY;
    let state = save(p);
    const intoNarrow: number[] = [];
    const intoWide: number[] = [];
    for (let move = 0; move < 10; move++) {
      const toNarrow = move % 2 === 0;
      const next = arrive(state, toNarrow);
      (toNarrow ? intoNarrow : intoWide).push(window.scrollY);
      state = save(next);
    }
    expect(intoWide).toEqual(Array(5).fill(wide));
    expect(intoNarrow).toEqual(Array(5).fill(intoNarrow[0]));
  });

  it("hands over the line read at the offset once the reader has scrolled away from the line placed", () => {
    const p = page();
    p.onView(view(101));
    render(p);
    p.onFocus(true);
    const placed = p.readingLine();
    expect(placed).toBe(100);
    setScroll(window.scrollY + 300);
    expect(p.readingLine()).toBeGreaterThan(placed + 4);
  });
});
