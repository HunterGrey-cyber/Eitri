import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { GENERIC_ROWS } from "../../../../../plugin-kit/keys/src/rows";
import type { InstallOptions } from "../kit";
import type { Bridge, EditorView } from "../protocol";
import type { MermaidRunner } from "../render/mermaid";
import { installFakeLayout, type FakeLayout } from "./fakelayout";
import { installKeys, PAGE_CONTEXT, PAGE_ROWS, pageRows } from "./keys";
import { Preview } from "./preview";
import { isStroke, OPEN_CLASS, VIEWER_CONTEXT, VIEWER_HINT, VIEWER_TEXT } from "./viewer";

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const FENCE = "`".repeat(3);
const PNG = "data:image/png;base64,iVBORw0KGgo=";
function view(over: Partial<EditorView> = {}): EditorView {
  return { buf: 1, name: "/p/notes.md", filetype: "markdown", tick: 1, current: true, line_count: 80, top: 1,
    top_fraction: 0, bottom: 20, cursor: { line: 1, col: 0 }, cause: "user", doc: DOC, ...over };
}
/* A document with a diagram whose source names its width (`W2000`: a 2000 x 500 drawing) and an image. */
function doc(o: { above?: string; source?: string; image?: boolean } = {}): string {
  return `${o.above ?? ""}# Title\n\npara\n\n${FENCE}mermaid\n${o.source ?? "graph LR W2000\nA-->B"}\n${FENCE}\n\n` +
    (o.image === false ? "" : `![pic](${PNG})\n\n`) + "tail\n";
}

let layout: FakeLayout;
let preview: Preview;
let installed: InstallOptions | null;
let calls: { verb: string; args: unknown }[];
// Sources the fake runner leaves waiting to be drawn, and those it fails.
let waiting: Set<string>;
let failing: Set<string>;
let drawn: number;
const kitKeys: Record<string, string[]> = { "find.start": ["/"], "scroll.down": ["<C-e>"], "history.back": ["H", "<C-o>"] };
const kit = { version: 5, install: vi.fn(async (o: InstallOptions) => { installed = o; return () => {}; }), setCurrent: vi.fn(),
  message: vi.fn(), status: vi.fn(), keys: vi.fn((a: string) => kitKeys[a] ?? []), findRefresh: vi.fn(), findSeed: vi.fn() };
const press = (action: string, key = "") => installed!.run(action, new KeyboardEvent("keydown", { key }));
const typeCount = (digits: string) => {
  for (const d of digits) press(d === "0" ? "viewer.left_edge" : "count", d);
};

const runner: MermaidRunner = {
  fill(root) {
    let n = 0;
    root.querySelectorAll<HTMLElement>("[data-eitri-mermaid]").forEach((el) => {
      const source = el.getAttribute("data-mdp-source") ?? el.textContent ?? "";
      el.setAttribute("data-mdp-source", source);
      // The fence's content ends with its line break; the sets name the sources without it.
      const name = source.trim();
      if (waiting.has(name)) {
        el.classList.add("mdp-diagram-pending");
        el.removeAttribute("data-mdp-key");
        el.textContent = "rendering diagram";
        return;
      }
      el.classList.remove("mdp-diagram-pending");
      el.setAttribute("data-mdp-key", source);
      if (failing.has(name)) {
        el.classList.add("mdp-box");
        el.innerHTML = "<pre><code>bad</code></pre><p>Parse error</p>";
        return;
      }
      const w = /W(\d+)/.exec(source)?.[1] ?? "2000";
      el.innerHTML = `<svg id="d${++drawn}" viewBox="0 0 ${w} 500" style="max-width: ${w}px"><style>#d${drawn} g{}</style><g></g></svg>`;
      n++;
    });
    return n;
  },
  renderPending: () => {},
  setTheme: () => {},
};

const stageSize = { w: 800, h: 600 };
// The size an image has loaded at (400 x 300 unless a test says otherwise).
let natural = { w: 400, h: 300 };
const props: [string, PropertyDescriptor | undefined][] = [];
function stub(proto: object, name: string, get: (this: HTMLElement) => unknown): void {
  props.push([name, Object.getOwnPropertyDescriptor(proto, name)]);
  Object.defineProperty(proto, name, { configurable: true, get });
}

beforeEach(async () => {
  vi.useFakeTimers();
  (window as { requestAnimationFrame?: unknown }).requestAnimationFrame = undefined;
  document.body.innerHTML = '<div id="mdp-notice"></div><main id="mdp-root" class="mdp-root"></main>';
  layout = installFakeLayout(document.getElementById("mdp-root")!);
  document.getElementById("mdp-root")!.style.lineHeight = "8.8px";
  (window as unknown as { eitriKeys: typeof kit }).eitriKeys = kit;
  installed = null;
  calls = [];
  waiting = new Set();
  failing = new Set();
  drawn = 0;
  for (const f of [kit.install, kit.message, kit.status]) f.mockClear();
  // jsdom lays nothing out: the viewer's stage is 800 x 600, and an image has loaded at 400 x 300.
  stub(HTMLElement.prototype, "clientWidth", function () { return this.classList.contains("mdp-viewer-stage") ? stageSize.w : 0; });
  stub(HTMLElement.prototype, "clientHeight", function () { return this.classList.contains("mdp-viewer-stage") ? stageSize.h : 0; });
  natural = { w: 400, h: 300 };
  stub(HTMLImageElement.prototype, "naturalWidth", () => natural.w);
  stub(HTMLImageElement.prototype, "naturalHeight", () => natural.h);
  stub(HTMLImageElement.prototype, "complete", () => true);
  const bridge: Bridge = { call: async (verb, args) => { calls.push({ verb, args }); return { applied: true }; }, on: () => () => {} };
  preview = new Preview(document.getElementById("mdp-root")!, bridge, { auto: false, remote_images: "https", sync: "both" }, runner);
  vi.spyOn(console, "info").mockImplementation(() => {});
  preview.onFocus(true);
  removers.push(await installKeys(preview, { run: (a) => a === "block.yank" && (yanks++, true), prefix: "Ctrl+b" }));
  yanks = 0;
});
let yanks = 0;
const removers: (() => void)[] = [];
afterEach(() => {
  for (const remove of removers.splice(0)) remove();
  for (const [name, d] of props.splice(0).reverse()) {
    const proto = name.startsWith("natural") || name === "complete" ? HTMLImageElement.prototype : HTMLElement.prototype;
    if (d) Object.defineProperty(proto, name, d);
    else delete (proto as unknown as Record<string, unknown>)[name];
  }
  preview.dispose(); layout.restore(); vi.useRealTimers(); vi.restoreAllMocks();
});

let tick = 1;
function show(body: string, buf = 1): void {
  preview.onView(view({ buf }));
  preview.onText({ buf, tick: ++tick, name: "/p/notes.md", text: body });
  vi.advanceTimersByTime(0);
}
const root = () => document.getElementById("mdp-root")!;
const diagram = () => root().querySelector<HTMLElement>("[data-eitri-mermaid]")!;
const overlay = () => document.querySelector<HTMLElement>(".mdp-viewer");
const shownSvg = () => document.querySelector<SVGSVGElement>(".mdp-viewer-stage > svg");
const shownImg = () => document.querySelector<HTMLImageElement>(".mdp-viewer-stage > img");
const viewBox = () => shownSvg()!.getAttribute("viewBox")!.split(" ").map(Number);
const top = (el: Element) => el.getBoundingClientRect().top + window.scrollY;
function cursorTo(el: Element): void {
  expect(preview.jumpCursorTo(top(el) + 0.5)).toBe(true);
}
function openDiagram(o: Parameters<typeof doc>[0] = {}): void {
  show(doc(o));
  cursorTo(diagram());
  press("viewer.open");
  expect(overlay()).not.toBeNull();
}

describe("the viewer's keys", () => {
  it("are `o` on the page and the zathura/nsxiv keys in the viewer's context", () => {
    const rows = new Map(PAGE_ROWS.map((r) => [r.action, r]));
    expect(rows.get("viewer.open")).toMatchObject({ keys: ["o"], context: PAGE_CONTEXT });
    const own = PAGE_ROWS.filter((r) => r.context === VIEWER_CONTEXT).map((r) => [r.action, r.keys]);
    expect(own).toEqual([
      ["viewer.close", ["q"]], ["viewer.left", ["h"]], ["viewer.right", ["l"]], ["viewer.left_edge", ["0"]],
      ["viewer.right_edge", ["$"]], ["viewer.zoom_in", ["+"]], ["viewer.zoom_out", ["-"]], ["viewer.actual", ["="]],
      ["viewer.fit", ["a"]], ["viewer.fit_width", ["s"]], ["count", ["1", "2", "3", "4", "5", "6", "7", "8", "9"]],
    ]);
    expect(VIEWER_HINT).toContain("q close");
  });

  it("clash with no other key of the page or of the kit's own rows, and survive every prefix", () => {
    const all = PAGE_ROWS.flatMap((r) => r.keys);
    expect(new Set(all).size).toBe(all.length);
    const kitKeysAll = new Set(GENERIC_ROWS.flatMap((r) => r.keys));
    const viewerKeys = PAGE_ROWS.filter((r) => r.context === VIEWER_CONTEXT || r.action === "viewer.open").flatMap((r) => r.keys);
    expect(viewerKeys.filter((k) => kitKeysAll.has(k))).toEqual([]);
    // No Ctrl key among them, so no prefix (or none named) costs the viewer a key.
    for (const prefix of [undefined, "Ctrl+b", "Ctrl+a"]) {
      const kept = pageRows(prefix).flatMap((r) => r.keys);
      for (const k of viewerKeys) expect(kept).toContain(k);
    }
  });

  it("do nothing while the viewer is closed, except `o`", () => {
    show(doc());
    for (const a of ["viewer.close", "viewer.left", "viewer.zoom_in", "viewer.actual", "viewer.fit", "count"]) {
      expect(press(a, "5")).toBe(false);
    }
    expect(overlay()).toBeNull();
  });

  it("match a single stroke of the kit's notation", () => {
    expect(isStroke("/", new KeyboardEvent("keydown", { key: "/" }))).toBe(true);
    expect(isStroke("H", new KeyboardEvent("keydown", { key: "h" }))).toBe(false);
    expect(isStroke("<C-e>", new KeyboardEvent("keydown", { key: "e", ctrlKey: true }))).toBe(true);
    expect(isStroke("<C-e>", new KeyboardEvent("keydown", { key: "e" }))).toBe(false);
    expect(isStroke("gg", new KeyboardEvent("keydown", { key: "g" }))).toBe(false);
  });
});

describe("the page's scroll bar", () => {
  const hidden = () => document.documentElement.classList.contains(OPEN_CLASS);

  it("is hidden while the viewer is open and back when it closes by a key", () => {
    show(doc());
    expect(hidden()).toBe(false);
    cursorTo(diagram());
    press("viewer.open");
    expect(hidden()).toBe(true);
    press("viewer.close");
    expect(hidden()).toBe(false);
    press("viewer.open");
    press("mode.escape");
    expect(hidden()).toBe(false);
  });

  it("comes back when an update closes the viewer, and when the keys are removed", () => {
    openDiagram();
    expect(hidden()).toBe(true);
    show("# Title\n\npara\n\ntail\n");
    expect(overlay()).toBeNull();
    expect(hidden()).toBe(false);
    openDiagram();
    expect(hidden()).toBe(true);
    for (const remove of removers.splice(0)) remove();
    expect(overlay()).toBeNull();
    expect(hidden()).toBe(false);
  });
});

describe("opening and closing", () => {
  it("`o` on a diagram shows a copy of its drawn SVG, fitted, the page's own diagram untouched", () => {
    openDiagram();
    const original = diagram().querySelector("svg")!;
    const copy = shownSvg()!;
    expect(copy).not.toBe(original);
    expect(copy.id).toBe(original.id);
    expect(copy.querySelector("style")).not.toBeNull();
    expect(copy.getAttribute("style")).toBeNull();
    expect(original.getAttribute("style")).toBe("max-width: 2000px");
    // 2000 x 500 in 800 x 600 fits at 0.4: the whole width, centred down.
    expect(viewBox()).toEqual([0, -500, 2000, 1500]);
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("40%");
    expect(document.querySelector(".mdp-viewer-keys")!.textContent).toBe(VIEWER_HINT);
    expect(overlay()!.parentElement).toBe(document.body);
  });

  it("`o` says why when the cursor is on no diagram, or on one not drawn", () => {
    show(doc());
    cursorTo(root().querySelector("p")!);
    press("viewer.open");
    expect(overlay()).toBeNull();
    expect(kit.message).toHaveBeenLastCalledWith(VIEWER_TEXT.nothing);
    waiting.add("graph LR W2000\nA-->B");
    show(doc({ above: "x\n\n" }));
    cursorTo(diagram());
    press("viewer.open");
    expect(overlay()).toBeNull();
    expect(kit.message).toHaveBeenLastCalledWith(VIEWER_TEXT.notDrawn);
  });

  it("`q` and `<Esc>` close it; nothing of it is left on the page", () => {
    openDiagram();
    press("viewer.close");
    expect(overlay()).toBeNull();
    press("viewer.open");
    expect(overlay()).not.toBeNull();
    press("mode.escape");
    expect(overlay()).toBeNull();
    expect(document.querySelector(".mdp-viewer-stage")).toBeNull();
  });

  it("`<Esc>` first drops a count being typed, as vim does", () => {
    openDiagram();
    typeCount("5");
    expect(document.querySelector(".mdp-viewer-count")!.textContent).toBe("5");
    press("mode.escape");
    expect(overlay()).not.toBeNull();
    expect(document.querySelector(".mdp-viewer-count")!.textContent).toBe("");
    press("mode.escape");
    expect(overlay()).toBeNull();
  });

  it("closing puts the cursor back on the diagram when an update moved the text under it", () => {
    openDiagram();
    // Four lines above: the cursor keeps its source line, which is now the paragraph's.
    show(doc({ above: "a\n\nb\n\n" }));
    const band = () => preview.cursorBand()!;
    expect(band().top >= top(diagram()) + diagram().getBoundingClientRect().height || band().bottom <= top(diagram())).toBe(true);
    press("viewer.close");
    const d = diagram().getBoundingClientRect();
    expect(band().top < d.bottom + window.scrollY && band().bottom > d.top + window.scrollY).toBe(true);
  });

  it("Enter closes it and edits at the diagram's line", async () => {
    openDiagram();
    press("edit.here");
    expect(overlay()).toBeNull();
    await vi.advanceTimersByTimeAsync(0);
    const j = calls.find((c) => c.verb === "editor.jump")!.args as { line: number };
    // The fence opens on line 5 (one-based); its first code line is 6.
    expect(j.line).toBeGreaterThanOrEqual(5);
    expect(j.line).toBeLessThanOrEqual(7);
  });
});

describe("moving and zooming by keys", () => {
  it("zooms around the centre in steps, a count multiplying them, and `=` goes to 100% or the count's percent", () => {
    openDiagram();
    press("viewer.zoom_in");
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("50%");
    typeCount("2");
    press("viewer.zoom_in");
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("78%");
    press("viewer.zoom_out");
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("63%");
    press("viewer.actual");
    expect(viewBox()[2]).toBeCloseTo(800);
    typeCount("200");
    press("viewer.actual");
    expect(viewBox()[2]).toBeCloseTo(400);
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("200%");
  });

  it("pans a fifth of the view per `hjkl`, `5l` five of them, and stops at the edges", () => {
    openDiagram();
    press("viewer.actual"); // 100%: 800 x 600 of 2000 x 500, centred at x 600
    const x0 = viewBox()[0];
    expect(x0).toBeCloseTo(600);
    press("viewer.right", "l");
    expect(viewBox()[0]).toBeCloseTo(x0 + 160);
    press("viewer.left", "h");
    typeCount("5");
    press("viewer.right", "l");
    expect(viewBox()[0]).toBeCloseTo(1200);
    press("viewer.left_edge");
    expect(viewBox()[0]).toBe(0);
    press("viewer.right_edge");
    expect(viewBox()[0]).toBe(1200);
    // Taller than wide at 2x: j/k, d/u, gg/G move down the picture.
    typeCount("400");
    press("viewer.actual");
    press("cursor.first");
    expect(viewBox()[1]).toBe(0);
    press("cursor.down", "j");
    expect(viewBox()[1]).toBeCloseTo(0.2 * 150);
    press("cursor.half_down", "d");
    expect(viewBox()[1]).toBeCloseTo(30 + 75);
    press("cursor.last");
    expect(viewBox()[1]).toBeCloseTo(500 - 150);
    press("cursor.half_up", "u");
    expect(viewBox()[1]).toBeCloseTo(350 - 75);
    press("cursor.page_up");
    expect(viewBox()[1]).toBeCloseTo(275 - 150);
  });

  it("`0` after a digit is part of the count", () => {
    openDiagram();
    press("viewer.actual");
    typeCount("10");
    expect(document.querySelector(".mdp-viewer-count")!.textContent).toBe("10");
    press("viewer.zoom_out");
    // Ten steps out reach the fitted 40%.
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("40%");
  });

  it("`a` fits the whole picture and `s` its width", () => {
    show(doc({ source: "graph TD W400" }));
    cursorTo(diagram());
    press("viewer.open");
    // 400 x 500 fits at 1.2 by its height.
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("120%");
    press("viewer.fit_width");
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("200%");
    expect(viewBox()[1]).toBe(0);
    press("viewer.fit");
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("120%");
  });

  it("takes every other page key and does nothing with it", () => {
    openDiagram();
    const before = viewBox().join(" ");
    for (const a of ["block.next", "heading.next", "scroll.right", "block.yank", "viewer.open"]) expect(press(a)).toBe(true);
    expect(viewBox().join(" ")).toBe(before);
    expect(yanks).toBe(0);
    expect(overlay()).not.toBeNull();
  });

  it("holds back the kit's single-stroke keys that would act on the page under it, and only while open", () => {
    show(doc());
    const seen: string[] = [];
    const listener = (ev: KeyboardEvent) => seen.push(ev.key);
    window.addEventListener("keydown", listener);
    const send = (key: string, ctrlKey = false) => document.body.dispatchEvent(new KeyboardEvent("keydown", { key, ctrlKey, bubbles: true }));
    try {
      cursorTo(diagram());
      press("viewer.open");
      send("/"); send("e", true); send("H"); send("l");
      expect(seen).toEqual(["l"]);
      press("viewer.close");
      send("/");
      expect(seen).toEqual(["l", "/"]);
    } finally {
      window.removeEventListener("keydown", listener);
    }
  });
});

describe("the pointer", () => {
  const stage = () => document.querySelector<HTMLElement>(".mdp-viewer-stage")!;
  const pointer = (type: string, x: number, y: number) => {
    const ev = new MouseEvent(type, { clientX: x, clientY: y, button: 0, bubbles: true, cancelable: true });
    Object.defineProperty(ev, "pointerId", { value: 1 });
    stage().dispatchEvent(ev);
  };

  it("the wheel pans, Ctrl+wheel zooms at the pointer", () => {
    openDiagram();
    press("viewer.actual");
    const x0 = viewBox()[0];
    stage().dispatchEvent(new WheelEvent("wheel", { deltaX: 100, deltaY: 0, cancelable: true }));
    expect(viewBox()[0]).toBeCloseTo(x0 + 100);
    // The picture point under the pointer stays under it.
    const px = 200;
    const under = viewBox()[0] + px * (viewBox()[2] / 800);
    stage().dispatchEvent(new WheelEvent("wheel", { deltaY: -300, ctrlKey: true, clientX: px, clientY: 300, cancelable: true }));
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe(`${Math.round(Math.E * 100)}%`);
    expect(viewBox()[0] + px * (viewBox()[2] / 800)).toBeCloseTo(under);
  });

  it("a drag pans, a double-click toggles the fitted view and twice it", () => {
    openDiagram();
    press("viewer.actual");
    const x0 = viewBox()[0];
    pointer("pointerdown", 400, 300);
    pointer("pointermove", 300, 300);
    pointer("pointerup", 300, 300);
    expect(viewBox()[0]).toBeCloseTo(x0 + 100);
    pointer("dblclick", 400, 300);
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("40%");
    pointer("dblclick", 400, 300);
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("80%");
  });
});

describe("the document updating under the viewer", () => {
  it("an edit elsewhere keeps the same drawing and the view", () => {
    openDiagram();
    press("viewer.zoom_in");
    const node = shownSvg();
    const box = viewBox().join(" ");
    show(doc({ above: "more\n\n" }));
    expect(overlay()).not.toBeNull();
    expect(shownSvg()).toBe(node);
    expect(viewBox().join(" ")).toBe(box);
  });

  it("an edit of the diagram redraws it, keeping the zoom and the centre's share of the picture", () => {
    openDiagram();
    press("viewer.actual");
    press("viewer.right", "l");
    const [x, , w] = viewBox();
    const share = (x + w / 2) / 2000;
    show(doc({ source: "graph LR W3000\nA-->B" }));
    expect(overlay()).not.toBeNull();
    const [x2, , w2] = viewBox();
    expect(w2).toBeCloseTo(w);
    expect((x2 + w2 / 2) / 3000).toBeCloseTo(share);
    expect(shownSvg()!.id).toBe(diagram().querySelector("svg")!.id);
  });

  it("a fitted view fits the redrawn diagram", () => {
    openDiagram();
    show(doc({ source: "graph LR W1000\nA-->B" }));
    // 1000 x 500 fits at 0.8 by its width.
    expect(document.querySelector(".mdp-viewer-zoom")!.textContent).toBe("80%");
  });

  it("keeps the last drawing while the new one waits, and says so when it fails", () => {
    openDiagram();
    const node = shownSvg();
    waiting.add("graph LR W2100\nA-->B");
    show(doc({ source: "graph LR W2100\nA-->B" }));
    expect(shownSvg()).toBe(node);
    failing.add("graph LR W2200\nA-->B");
    show(doc({ source: "graph LR W2200\nA-->B" }));
    expect(shownSvg()).toBe(node);
    expect(kit.message).toHaveBeenLastCalledWith(VIEWER_TEXT.stale);
  });

  it("follows its own diagram when another one is added above it", () => {
    openDiagram();
    show(doc({ above: `${FENCE}mermaid\ngraph LR W900\nC-->D\n${FENCE}\n\n` }));
    expect(overlay()).not.toBeNull();
    expect(viewBox()[2]).toBeCloseTo(2000);
  });

  it("closes with a notice when the diagram is gone, or the preview shows another buffer", () => {
    openDiagram();
    show("# Title\n\npara\n\ntail\n");
    expect(overlay()).toBeNull();
    expect(kit.message).toHaveBeenLastCalledWith(VIEWER_TEXT.gone);
    openDiagram();
    show(doc(), 2);
    expect(overlay()).toBeNull();
  });
});

describe("an image", () => {
  const img = () => root().querySelector<HTMLImageElement>("img")!;

  const zoom = () => document.querySelector(".mdp-viewer-zoom")!.textContent;

  it("opens with the same src, at 100% when smaller than the viewer, sized in screen px rather than scaled", () => {
    show(doc());
    cursorTo(img());
    press("viewer.open");
    expect(shownImg()!.getAttribute("src")).toBe(img().getAttribute("src"));
    // 400 x 300 would fit 800 x 600 at 2; an image opens no larger than its own size (nsxiv's "down").
    expect(zoom()).toBe("100%");
    expect(shownImg()!.style.width).toBe("400px");
    expect(shownImg()!.style.height).toBe("300px");
    expect(shownImg()!.style.left).toBe("200px");
    expect(shownImg()!.style.transform).toBe("");
    // `a` still fits it, enlarging it.
    press("viewer.fit");
    expect(shownImg()!.style.width).toBe("800px");
  });

  it("opens a larger image fitted", () => {
    natural = { w: 1600, h: 1200 };
    show(doc());
    cursorTo(img());
    press("viewer.open");
    expect(zoom()).toBe("50%");
    expect(shownImg()!.style.width).toBe("800px");
  });

  it("double-click toggles between the view it opened at and twice it", () => {
    show(doc());
    cursorTo(img());
    press("viewer.open");
    const stage = document.querySelector<HTMLElement>(".mdp-viewer-stage")!;
    const dbl = () => stage.dispatchEvent(new MouseEvent("dblclick", { clientX: 400, clientY: 300, bubbles: true, cancelable: true }));
    dbl();
    expect(zoom()).toBe("200%");
    dbl();
    expect(zoom()).toBe("100%");
    press("viewer.zoom_in");
    dbl();
    expect(zoom()).toBe("100%");
  });

  it("closes when the image leaves the document", () => {
    show(doc());
    cursorTo(img());
    press("viewer.open");
    show(doc({ image: false }));
    expect(overlay()).toBeNull();
    expect(kit.message).toHaveBeenLastCalledWith(VIEWER_TEXT.imageGone);
  });
});
