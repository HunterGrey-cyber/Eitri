import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView, PreviewSettings } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT } from "./notices";
import { Preview } from "./preview";

/* jsdom lays nothing out. Each tagged leaf block gets 20 px per source line (100 more for a paragraph saying TALL)
   and a 10 px gap, stacked in document order; the end sentinel gets 1 px. */
let root: HTMLElement;
function layOut(): Map<Element, { top: number; height: number }> {
  const pos = new Map<Element, { top: number; height: number }>();
  let y = 0;
  for (const el of root.querySelectorAll<HTMLElement>(".code-line")) {
    if (el.querySelector(".code-line")) continue;
    const line = Number(el.dataset.line);
    const height = el.dataset.lineEnd === undefined ? 1
      : (Number(el.dataset.lineEnd) - line) * 20 + (el.textContent?.includes("TALL") ? 100 : 0);
    pos.set(el, { top: y, height });
    y += height + 10;
  }
  return pos;
}
function setScroll(y: number): void {
  Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });
  window.dispatchEvent(new Event("scroll"));
}

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const paragraphs = (n: number, first = "p0") => Array.from({ length: n }, (_, i) => (i === 0 ? first : `p${i}`)).join("\n\n");
const text = (buf: number, body: string, tick = 1) => ({ buf, tick, name: `/p/${buf}.md`, text: body });
function view(buf: number, top: number, over: Partial<EditorView> = {}): EditorView {
  return { buf, name: `/p/${buf}.md`, filetype: "markdown", tick: 1, current: true, line_count: 60, top, top_fraction: 0,
    bottom: top + 20, cursor: { line: top, col: 0 }, cause: "user", doc: DOC, ...over };
}

let calls: { verb: string; args: unknown }[];
let made: Preview[] = [];
let runner: MermaidRunner;
function make(settings: Partial<PreviewSettings> = {}): Preview {
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied: true }; }, on: () => () => {} };
  const p = new Preview(root, bridge, { auto: false, remote_images: "https", sync: "both", ...settings }, runner);
  made.push(p);
  return p;
}
const reveals = () => calls.filter((c) => c.verb === "editor.reveal").map((c) => c.args);
function show(p: Preview, buf: number, body: string): void { p.onText(text(buf, body)); vi.advanceTimersByTime(0); }

beforeEach(() => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice" class="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  root = document.getElementById("mdp-root")!;
  calls = [];
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
  window.scrollTo = ((opts: ScrollToOptions) => setScroll(opts.top ?? 0)) as typeof window.scrollTo;
  window.scrollBy = ((opts: ScrollToOptions) => setScroll(window.scrollY + (opts.top ?? 0))) as typeof window.scrollBy;
  Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
  vi.spyOn(console, "info").mockImplementation(() => {});
});
afterEach(() => { for (const p of made.splice(0)) p.dispose(); vi.useRealTimers(); vi.restoreAllMocks(); });

describe("the preview", () => {
  it("shows the empty state, then the rendered buffer, rendering only the newest text", () => {
    const p = make();
    expect(document.getElementById("mdp-notice")!.textContent).toBe(EMPTY_TEXT);
    p.onView(view(1, 1));
    p.onText(text(1, "# old"));
    p.onText(text(1, "# new\n\npara", 2));
    vi.advanceTimersByTime(0);
    expect(runner.fill).toHaveBeenCalledTimes(1);
    expect(root.querySelector("h1")!.textContent).toBe("new");
    expect(document.getElementById("mdp-notice")!.textContent).toBe("");
    expect(runner.renderPending).toHaveBeenCalledWith(root, expect.any(Function));
  });

  it("follows nvim's top line when nvim leads, inside blocks and across gaps", () => {
    const p = make();
    p.onView(view(1, 21));
    show(p, 1, paragraphs(30));
    expect(window.scrollY).toBe(300);
    p.onView(view(1, 22));
    expect(window.scrollY).toBe(320);
    p.onView(view(1, 21, { top_fraction: 0.5 }));
    expect(window.scrollY).toBe(310);
    expect(reveals()).toEqual([]);
  });

  it("leads with the keys: its scroll sends throttled reveals when the line changes, and ignores its own echo", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1 }]);
    setScroll(305);
    vi.advanceTimersByTime(40);
    expect(reveals().length).toBe(1);
    setScroll(330);
    vi.advanceTimersByTime(40);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1 }, { buf: 1, line: 23, token: 2 }]);
    p.onView(view(1, 40, { cause: "reveal", token: 2 }));
    p.onView(view(1, 5));
    expect(window.scrollY).toBe(330);
  });

  it("lets a pointer scroll lead without the keys, until nvim's own next view", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    window.dispatchEvent(new Event("wheel"));
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1 }]);
    p.onView(view(1, 5));
    expect(window.scrollY).toBe(60);
  });

  it("never reveals with sync = editor, and applies no view with sync = off", () => {
    const editor = make({ sync: "editor" });
    editor.onView(view(1, 1));
    show(editor, 1, paragraphs(30));
    editor.onFocus(true);
    setScroll(300);
    vi.advanceTimersByTime(100);
    expect(reveals()).toEqual([]);
    editor.onView(view(1, 5));
    expect(window.scrollY).toBe(60);
  });

  it("applies nothing with sync = off after the buffer's first view", () => {
    const off = make({ sync: "off" });
    off.onView(view(1, 21));
    show(off, 1, paragraphs(30));
    expect(window.scrollY).toBe(300);
    off.onView(view(1, 5));
    expect(window.scrollY).toBe(300);
    setScroll(100);
    vi.advanceTimersByTime(100);
    expect(reveals()).toEqual([]);
  });

  it("keeps the same source line at the top when text above grows, whoever leads, without sending a reveal", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    vi.advanceTimersByTime(100);
    const sent = reveals().length;
    show(p, 1, paragraphs(30, "TALL p0"));
    expect(window.scrollY).toBe(400);
    vi.advanceTimersByTime(100);
    expect(reveals().length).toBe(sent);
    p.onFocus(false);
    p.onView(view(1, 21));
    show(p, 1, paragraphs(30));
    expect(window.scrollY).toBe(300);
  });

  it("applies a new buffer's first view whoever leads, once its text has rendered", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    p.onView(view(2, 11));
    show(p, 2, paragraphs(20));
    expect(window.scrollY).toBe(150);
    expect(p.followedBuf()).toBe(2);
  });

  it("shows the too-large, closed and editor notices", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, "# kept");
    p.onText({ buf: 1, tick: 2, name: "/p/1.md", too_large: { bytes: 5452595, lines: 61000, max_bytes: 4194304, max_lines: 50000 } });
    vi.advanceTimersByTime(0);
    expect(root.hidden).toBe(true);
    expect(document.getElementById("mdp-notice")!.textContent).toContain("5.2 MB, 61,000 lines");
    show(p, 1, "# back");
    expect(root.hidden).toBe(false);
    p.onClosed({ buf: 9, reason: "wiped" });
    expect(document.getElementById("mdp-notice")!.textContent).toBe("");
    p.onClosed({ buf: 1, reason: "wiped" });
    expect(document.getElementById("mdp-notice")!.textContent).toBe(BUFFER_CLOSED_TEXT);
    expect(root.querySelector("h1")!.textContent).toBe("back");
    p.onClosed({ buf: 1, reason: "editor" });
    expect(document.getElementById("mdp-notice")!.textContent).toBe(EDITOR_CLOSED_TEXT);
  });

  it("renders again when the document route arrives and when asked to", () => {
    const p = make();
    p.onView(view(1, 1, { doc: null }));
    show(p, 1, "![x](x.png)");
    expect(root.querySelector('.mdp-box[data-eitri-blocked="unsaved"]')).not.toBeNull();
    p.onView(view(1, 1));
    vi.advanceTimersByTime(0);
    expect(root.querySelector("img")!.getAttribute("src")).toBe(`${DOC.base}x.png`);
    const before = (runner.fill as ReturnType<typeof vi.fn>).mock.calls.length;
    p.rerender();
    vi.advanceTimersByTime(0);
    expect((runner.fill as ReturnType<typeof vi.fn>).mock.calls.length).toBe(before + 1);
  });

  it("turns a document-route image that fails to load into an outside-the-project box, counted, on every render", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, "![out](link-out.png)\n\n![x](x.png)");
    const seen: string[] = [];
    p.afterUpdate((why) => { seen.push(why ?? ""); });
    root.querySelector('img[alt="out"]')!.dispatchEvent(new Event("error"));
    const box = root.querySelector('.mdp-box[data-eitri-blocked="outside"]')!;
    expect(box.querySelector(".mdp-box-reason")!.textContent).toBe("outside the project");
    expect(box.textContent).toContain("out");
    expect(root.querySelectorAll("img")).toHaveLength(1);
    expect(p.blockedCount()).toBe(1);
    expect(seen).toContain("anchor");
    show(p, 1, "![out](link-out.png)\n\n![x](x.png)\n\nmore");
    expect(root.querySelector('img[alt="out"]')).toBeNull();
    expect(p.blockedCount()).toBe(1);
    // An image outside the document route (a data: URL) that fails is left to the engine as it is.
    show(p, 1, "![d](data:image/png;base64,AAAA)");
    root.querySelector('img[alt="d"]')!.dispatchEvent(new Event("error"));
    expect(root.querySelector('img[alt="d"]')).not.toBeNull();
  });

  it("marks nvim's cursor line beside its block while nvim leads", () => {
    const p = make();
    p.onView(view(1, 1, { cursor: { line: 21, col: 0 } }));
    show(p, 1, paragraphs(30));
    const bar = document.querySelector<HTMLElement>(".mdp-cursor-bar")!;
    expect(bar.hidden).toBe(false);
    expect([bar.style.top, bar.style.height]).toEqual(["300px", "20px"]);
    p.onFocus(true);
    expect(bar.hidden).toBe(true);
  });

  it("tells its listeners after each render and view", () => {
    const p = make();
    const fn = vi.fn();
    const off = p.afterUpdate(fn);
    p.onView(view(1, 1));
    show(p, 1, "x");
    expect(fn).toHaveBeenCalledTimes(2);
    off();
    p.onView(view(1, 1));
    expect(fn).toHaveBeenCalledTimes(2);
  });
});

describe("the anchor and the reveals across a change of lead", () => {
  // The reader scrolls the preview with the pointer to line 21, then nvim's own view takes the preview to line 5.
  function leadThenFollow(): Preview {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    window.dispatchEvent(new Event("wheel"));
    setScroll(300);
    p.onView(view(1, 5));
    expect(window.scrollY).toBe(60);
    return p;
  }

  it("keeps the line shown when the preview takes the keys, through a rerender and a height change", () => {
    const p = leadThenFollow();
    p.onFocus(true);
    expect(window.scrollY).toBe(60);
    p.rerender();
    vi.advanceTimersByTime(0);
    expect(window.scrollY).toBe(60);
    root.dispatchEvent(new Event("load"));
    expect(window.scrollY).toBe(60);
  });

  it("gives nvim back the line shown, not the reader's line from an earlier lead", () => {
    const p = leadThenFollow();
    p.onFocus(true);
    p.onFocus(false);
    show(p, 1, paragraphs(30));
    expect(window.scrollY).toBe(60);
  });

  it("reveals a line it revealed before once nvim has moved away from it", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    vi.advanceTimersByTime(100);
    p.onFocus(false);
    p.onView(view(1, 19));
    expect(window.scrollY).toBe(270);
    vi.advanceTimersByTime(100);
    p.onFocus(true);
    setScroll(300);
    vi.advanceTimersByTime(100);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1 }, { buf: 1, line: 21, token: 2 }]);
  });

  it("hides the cursor bar with the document when the text is too large", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(3));
    const bar = document.querySelector<HTMLElement>(".mdp-cursor-bar")!;
    expect(bar.hidden).toBe(false);
    p.onText({ buf: 1, tick: 2, name: "/p/1.md", too_large: { bytes: 5452595, lines: 61000, max_bytes: 4194304, max_lines: 50000 } });
    vi.advanceTimersByTime(0);
    expect(bar.hidden).toBe(true);
  });
});
