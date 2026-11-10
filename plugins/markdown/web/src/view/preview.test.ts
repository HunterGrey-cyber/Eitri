import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Bridge, EditorView, PreviewSettings } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { currentBlock, noteClick } from "./current";
import { installFakeRows } from "./fakelayout";
import { BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT } from "./notices";
import { Preview } from "./preview";

/* jsdom lays nothing out. Each tagged leaf block gets 20 px per source line (100 more for a paragraph saying TALL)
   and a 10 px gap, stacked in document order; the end sentinel gets 1 px. Its text fills it in rows of 20 px. */
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
  // The reading reserve is at least five lines of the page's text; these tests are written around a 44 px floor, so
  // the page's text is 8.8 px a line here (the 22 px of the page's own stylesheet: the reserve's own tests).
  root.style.lineHeight = "8.8px";
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
  unrows = installFakeRows((el) => {
    const pos = layOut();
    for (let e: Element | null = el; e; e = e.parentElement) { const p = pos.get(e); if (p) return p; }
    return undefined;
  });
  vi.spyOn(console, "info").mockImplementation(() => {});
});
let unrows: () => void = () => {};
afterEach(() => { for (const p of made.splice(0)) p.dispose(); unrows(); vi.useRealTimers(); vi.restoreAllMocks(); });

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

  it("leads with the keys: a scroll pushes the cursor below the top margin and sends throttled reveals when a line changes, and ignores its own echo", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    // The cursor starts on nvim's line 1 (block 0); a scroll to 300 pushes it to the first row at least the 44 px
    // reserve below the top: block 12 at 360, line 25.
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }]);
    setScroll(305);
    vi.advanceTimersByTime(40);
    expect(reveals().length).toBe(1);
    setScroll(330);
    vi.advanceTimersByTime(40);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }, { buf: 1, line: 23, token: 2, cursor: 27 }]);
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
    // Without the keys the reveal carries the top alone: nvim's cursor moves no further than 'scrolloff' needs, and
    // a scroll that keeps the top line sends nothing.
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1 }]);
    setScroll(305);
    vi.advanceTimersByTime(40);
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

  it("turns a document-route image that fails to load into an outside-the-project box, counted, until its text changes", () => {
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
    p.rerender();
    vi.advanceTimersByTime(0);
    expect(root.querySelector('img[alt="out"]')).toBeNull();
    expect(p.blockedCount()).toBe(1);
    // A host update with unchanged text also keeps the failed URL boxed.
    p.onText(text(1, "![out](link-out.png)\n\n![x](x.png)", 2));
    vi.advanceTimersByTime(0);
    expect(root.querySelector('img[alt="out"]')).toBeNull();
    show(p, 1, "![out](link-out.png)\n\n![x](x.png)\n\nmore");
    expect(root.querySelector('img[alt="out"]')).not.toBeNull();
    expect(p.blockedCount()).toBe(0);
    root.querySelector('img[alt="out"]')!.dispatchEvent(new Event("load"));
    p.rerender();
    vi.advanceTimersByTime(0);
    expect(root.querySelector('img[alt="out"]')).not.toBeNull();
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
    const band = document.querySelector<HTMLElement>(".mdp-cursor-line")!;
    expect(bar.hidden).toBe(false);
    expect(band.hidden).toBe(true);
    expect([bar.style.top, bar.style.height]).toEqual(["300px", "20px"]);
    // With the keys the bar is the preview's own cursor, which starts on nvim's line, with the row band under it.
    p.onFocus(true);
    expect([bar.hidden, band.hidden]).toEqual([false, false]);
    expect([bar.style.top, bar.style.height, band.style.top, band.style.height]).toEqual(["300px", "20px", "300px", "20px"]);
    p.moveRows(1);
    expect([bar.style.top, band.style.top]).toEqual(["330px", "330px"]);
    p.onFocus(false);
    expect([bar.style.top, band.hidden]).toEqual(["300px", true]);
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
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }, { buf: 1, line: 21, token: 2, cursor: 25 }]);
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

describe("the reading line shared with nvim", () => {
  it("keeps 'scrolloff' in nvim's rows below the top, at least five lines of text, at most a quarter of the view", () => {
    root.style.lineHeight = "22px";
    const p = make();
    // nvim says no 'scrolloff', or 0 (nvim's default): five lines of the page's text, never the top edge.
    p.onView(view(1, 1));
    expect(p.reservePx()).toBe(110);
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("110px");
    p.onView(view(1, 1, { scrolloff: 0 }));
    setScroll(200);
    expect(p.readingY()).toBe(310);
    // Above the floor, 'scrolloff' counts the page's lines from a host that does not say nvim's rows ...
    p.onView(view(1, 1, { scrolloff: 6 }));
    expect(p.reservePx()).toBe(132);
    // ... and nvim's rows when it does: the view's 768 px over nvim's 48 rows is 16 px a row.
    p.onView(view(1, 1, { scrolloff: 6, rows: 48, cursor_row: 1 }));
    expect(p.reservePx()).toBe(110);
    p.onView(view(1, 1, { scrolloff: 9, rows: 48, cursor_row: 1 }));
    expect(p.reservePx()).toBe(144);
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("144px");
    expect(p.readingY()).toBe(344);
    // nvim's rows changing alone (a resized screen) moves it too.
    p.onView(view(1, 1, { scrolloff: 9, rows: 64, cursor_row: 1 }));
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("110px");
    p.onView(view(1, 1, { scrolloff: 50 }));
    expect(p.reservePx()).toBe(window.innerHeight / 4);
  });

  it("follows the page's text size: five lines of it, refreshed when the size changes and nothing else does", () => {
    root.style.lineHeight = "22px";
    const p = make();
    p.onView(view(1, 1));
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("110px");
    // A larger editor font: the host's theme event sets the size, the line height follows from it.
    root.style.lineHeight = "29.34px";
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("110px");
    p.onTextSize();
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("147px");
    expect(p.reservePx()).toBeCloseTo(146.7, 5);
    root.style.lineHeight = "22px";
    p.onTextSize();
    expect(document.documentElement.style.getPropertyValue("--mdp-reserve")).toBe("110px");
  });

  it("goes to nvim as the reveal's cursor, and a change of the cursor's line alone sends a reveal", () => {
    const p = make();
    p.onView(view(1, 1, { scrolloff: 0 }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    // 'scrolloff' 0 keeps the floor, 44 px here: the cursor goes below 344, to line 25 (the block at 360).
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }]);
    // nvim's own view says 'scrolloff' 9 now, nine lines of 8.8 px: the next scroll pushes the cursor a block down,
    // the top stays.
    p.onView(view(1, 21, { scrolloff: 9, cause: "reveal", token: 1 }));
    setScroll(301);
    vi.advanceTimersByTime(40);
    expect(reveals().at(-1)).toEqual({ buf: 1, line: 21, token: 2, cursor: 27 });
    // A move of the cursor alone, with no scroll, sends one too.
    p.moveRows(1);
    vi.advanceTimersByTime(40);
    expect(reveals().at(-1)).toEqual({ buf: 1, line: 21, token: 3, cursor: 29 });
  });

  it("stops sending the cursor once the host refuses it", async () => {
    const refusing: Bridge = {
      call: async (verb, args) => {
        calls.push({ verb, args });
        if (verb === "editor.reveal" && (args as { cursor?: number }).cursor !== undefined) {
          throw Object.assign(new Error("unknown field `cursor`"), { code: "bad_request" });
        }
        return { applied: true };
      },
      on: () => () => {},
    };
    const p = new Preview(root, refusing, { auto: false, remote_images: "https", sync: "both" }, runner);
    made.push(p);
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    await vi.advanceTimersByTimeAsync(0);
    // The refused reveal moved nothing, so its place goes again without the cursor.
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }, { buf: 1, line: 21, token: 2 }]);
    setScroll(400);
    vi.advanceTimersByTime(40);
    expect(reveals().at(-1)).toEqual({ buf: 1, line: 27, token: 3 });
    expect(reveals().length).toBe(3);
  });

  it("makes nvim's cursor block the current block when the keys come in and it is on screen", () => {
    const p = make();
    p.onView(view(1, 1, { cursor: { line: 21, col: 0 } }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    expect(currentBlock(p)?.line).toBe(20);
    // A scroll that leaves the cursor inside the margins leaves it, and its block, where they were.
    setScroll(50);
    expect(currentBlock(p)?.line).toBe(20);
    // One that would take it past the top margin pushes it down.
    setScroll(280);
    expect(currentBlock(p)?.line).toBe(22);
  });

  it("leaves the reading point current when nvim's cursor block is off screen", () => {
    // With sync off nothing brings nvim's cursor into the preview's view.
    const p = make({ sync: "off" });
    p.onView(view(1, 1, { cursor: { line: 59, col: 0 } }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    expect(currentBlock(p)?.line).toBe(0);
  });

  it("reads the first block at the document's start, the reserve growing as the page scrolls", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    expect(p.readingY()).toBe(0);
    setScroll(10);
    expect(p.readingY()).toBe(20);
    setScroll(100);
    expect(p.readingY()).toBe(144);
  });

  it("lets a click that just happened win over nvim's cursor block", () => {
    const p = make();
    p.onView(view(1, 1, { cursor: { line: 21, col: 0 } }));
    show(p, 1, paragraphs(30));
    const box = p.index().find((b) => b.line === 4)!;
    noteClick();
    p.cursorToPoint(box.top + 5);
    p.onFocus(true);
    expect(currentBlock(p)?.line).toBe(4);
  });

  it("keeps its own place, inside a block, when nvim takes the lead back without having moved", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30, "TALL p0"));
    p.onFocus(true);
    // Inside the tall first block: the top line is 0, 70 % of the way through it.
    setScroll(84);
    expect(reveals().at(-1)).toMatchObject({ line: 1 });
    p.onFocus(false);
    p.onView(view(1, 1, { cursor: { line: 3, col: 0 } }));
    expect(window.scrollY).toBe(84);
    // nvim scrolled: its top goes.
    p.onView(view(1, 3));
    expect(window.scrollY).toBe(130);
    // And a later view at the old top is nvim's own top again.
    p.onView(view(1, 1));
    expect(window.scrollY).toBe(0);
  });

  it("finishes a running glide and tells nvim the final place when the keys leave", () => {
    const settle = vi.fn(() => setScroll(600));
    (window as unknown as { eitriKeys: unknown }).eitriKeys = { version: 4, settle };
    try {
      const p = make();
      p.onView(view(1, 1));
      show(p, 1, paragraphs(30));
      p.onFocus(true);
      setScroll(300);
      const before = reveals().length;
      p.onFocus(false);
      expect(settle).toHaveBeenCalled();
      expect(reveals().length).toBe(before + 1);
      expect(reveals().at(-1)).toMatchObject({ line: 41 });
      // nvim's echo, then its first own view at that top, leave the preview where it is.
      p.onView(view(1, 41, { cause: "reveal", token: (reveals().at(-1) as { token: number }).token }));
      p.onView(view(1, 41, { cursor: { line: 44, col: 0 } }));
      expect(window.scrollY).toBe(600);
    } finally {
      delete (window as unknown as { eitriKeys?: unknown }).eitriKeys;
    }
  });

  it("sends nothing on a focus round trip in which the preview did not move", () => {
    const p = make();
    p.onView(view(1, 1, { cursor: { line: 21, col: 0 } }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    p.onFocus(false);
    expect(reveals()).toEqual([]);
    // Nor once the preview's last move has already gone to nvim.
    p.onFocus(true);
    setScroll(300);
    vi.advanceTimersByTime(100);
    const sent = reveals().length;
    p.onFocus(false);
    expect(reveals().length).toBe(sent);
  });

  it("sends the trailing place still due when the keys leave", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    setScroll(330);
    expect(reveals().length).toBe(1);
    p.onFocus(false);
    expect(reveals().at(-1)).toMatchObject({ line: 23, cursor: 27 });
    vi.advanceTimersByTime(100);
    expect(reveals().length).toBe(2);
  });

  it("sends nothing over an edit jump's cursor: its own owed place goes first, top alone", () => {
    const settle = vi.fn(() => setScroll(600));
    (window as unknown as { eitriKeys: unknown }).eitriKeys = { version: 4, settle, destination: () => 600 };
    try {
      const p = make();
      p.onView(view(1, 1));
      show(p, 1, paragraphs(30));
      p.onFocus(true);
      setScroll(300);
      const before = reveals().length;
      // Enter during a glide heading for 600: the glide ends there and that top goes without a cursor.
      p.beforeEditJump();
      expect(settle).toHaveBeenCalled();
      expect(reveals().slice(before)).toEqual([{ buf: 1, line: 41, token: 2 }]);
      // The page's own scroll events until the keys go reveal nothing, and the hand-off sends nothing.
      setScroll(620);
      vi.advanceTimersByTime(100);
      p.onFocus(false);
      expect(reveals().length).toBe(before + 1);
    } finally {
      delete (window as unknown as { eitriKeys?: unknown }).eitriKeys;
    }
  });

  it("reveals again on scrolling once an edit jump is refused", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    p.beforeEditJump();
    p.editJumpFailed();
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25 }]);
  });

  it("puts nvim's cursor line as far down the view as its row is down nvim's screen while nvim leads", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(60));
    // Line 41 (block 20, at 600) starts on row 13 of nvim's 48: a quarter down nvim's screen, so a quarter down the
    // view, wherever nvim's top is.
    p.onView(view(1, 30, { cursor: { line: 41, col: 0 }, rows: 48, cursor_row: 13 }));
    expect(window.scrollY).toBe(600 - window.innerHeight / 4);
    // The cursor two lines down and two rows down (block 21, at 630): the page follows the line, not nvim's top.
    p.onView(view(1, 30, { cursor: { line: 43, col: 0 }, rows: 48, cursor_row: 15 }));
    expect(window.scrollY).toBe(630 - (window.innerHeight * 14) / 48);
    // nvim scrolling its window under the cursor takes the line up with it.
    p.onView(view(1, 30, { cursor: { line: 43, col: 0 }, rows: 48, cursor_row: 3 }));
    expect(window.scrollY).toBe(630 - (window.innerHeight * 2) / 48);
    // Near the document's start the page stays at its top rather than keep the share.
    p.onView(view(1, 1, { cursor: { line: 3, col: 0 }, rows: 48, cursor_row: 3 }));
    expect(window.scrollY).toBe(0);
    // The cursor on nvim's top line, scrolled half out of the window ('smoothscroll'): its shown half starts on the
    // window's first row, row 5 here, so half way into line 41's 20 px block goes a twelfth down the view.
    p.onView(view(1, 41, { cursor: { line: 41, col: 0 }, top_fraction: 0.5, rows: 48, cursor_row: 5 }));
    expect(window.scrollY).toBe(600 + 10 - window.innerHeight / 12);
    expect(reveals()).toEqual([]);
  });

  it("asks nvim to start the cursor's line on the row as far down its screen as the line is down the view", () => {
    const p = make();
    p.onView(view(1, 1, { rows: 48, cursor_row: 1 }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    // The cursor pushed below 344: line 25 (block 12, at 360) starts 60 px down the 768 px view, row 5 of 48.
    setScroll(300);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25, row: 5 }]);
    // Without the keys (a pointer scroll) the reveal carries the top alone, as before.
    p.onFocus(false);
    window.dispatchEvent(new Event("wheel"));
    setScroll(600);
    vi.advanceTimersByTime(40);
    expect(reveals().at(-1)).toEqual({ buf: 1, line: 41, token: 2 });
  });

  it("stops sending the row once the host refuses it, and keeps the cursor", async () => {
    const refusing: Bridge = {
      call: async (verb, args) => {
        calls.push({ verb, args });
        if (verb === "editor.reveal" && (args as { row?: number }).row !== undefined) {
          throw Object.assign(new Error("unknown field `row`"), { code: "bad_request" });
        }
        return { applied: true };
      },
      on: () => () => {},
    };
    const p = new Preview(root, refusing, { auto: false, remote_images: "https", sync: "both" }, runner);
    made.push(p);
    p.onView(view(1, 1, { rows: 48, cursor_row: 1 }));
    show(p, 1, paragraphs(30));
    p.onFocus(true);
    setScroll(300);
    await vi.advanceTimersByTimeAsync(0);
    expect(reveals()).toEqual([{ buf: 1, line: 21, token: 1, cursor: 25, row: 5 }, { buf: 1, line: 21, token: 2, cursor: 25 }]);
    setScroll(400);
    vi.advanceTimersByTime(40);
    // The cursor pushed below 444: block 15, line 31.
    expect(reveals().at(-1)).toEqual({ buf: 1, line: 27, token: 3, cursor: 31 });
  });

  it("keeps nvim's cursor line in view while nvim leads, scrolling the least, and reveals nothing", () => {
    const p = make();
    p.onView(view(1, 1, { scrolloff: 2 }));
    show(p, 1, paragraphs(60));
    const view_ = window.innerHeight;
    // Line 61 (block 30, at 900) is below the view from the top.
    p.onView(view(1, 1, { scrolloff: 2, cursor: { line: 61, col: 0 } }));
    expect(window.scrollY).toBe(900 + 20 - view_ + 44);
    // Inside the view: nvim's top, as before.
    p.onView(view(1, 21, { scrolloff: 2, cursor: { line: 25, col: 0 } }));
    expect(window.scrollY).toBe(300);
    expect(reveals()).toEqual([]);
  });

  it("puts G's place with the last block at the bottom of the view", () => {
    const p = make();
    p.onView(view(1, 1));
    show(p, 1, paragraphs(60));
    // 60 blocks: the last ends at 59 * 30 + 20.
    expect(p.bottomY()).toBe(59 * 30 + 20 - window.innerHeight);
  });
});
