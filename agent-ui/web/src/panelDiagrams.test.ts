// @vitest-environment jsdom
import { existsSync, readFileSync } from "node:fs";
import { URL as NodeURL } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DiagramController, MAX_DRAWINGS, MAX_DOCUMENT_SVG_BYTES, MAX_SVG_BYTES } from "./diagrams";
import { codeBlockText, renderMarkdown, renderMarkdownRevealed, renderedText, streamMarkdown } from "./markdown";

import { copySelectionText, isChromeNode, type SelectionLike } from "./visual";

const source = "flowchart LR\nA --> B";
const svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 400"><text>A</text></svg>';
function reply(text = source) {
  const root = document.createElement("div");
  root.innerHTML = renderMarkdown("```mermaid\n" + text + "\n```\n");
  document.body.append(root);
  return root;
}
function decoded(root: HTMLElement) {
  return decodeURIComponent(root.querySelector("img")!.getAttribute("src")!.split(",").slice(1).join(","));
}
beforeEach(() => document.body.replaceChildren());
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
describe("closed fences", () => {
  it.each(["```mermaid", "~~~mermaid", "```mermaid extra"])("marks %s only after its closing line", (open) => {
    const close = open.startsWith("~") ? "~~~" : "```";
    expect(renderMarkdown(`${open}\n${source}\n${close}\n`)).toContain('data-code-diagram="1"');
    expect(streamMarkdown(null, `${open}\n${source}\n${close}`).tail).not.toContain("data-code-diagram");
    expect(streamMarkdown(null, `${open}\n${source}\n${close}\n`).tail).toContain("data-code-diagram");
  });
  it("does not mark an open fence, another language, or raw HTML", () => {
    for (const text of ["```mermaid\nA", "```Mermaid\nA\n```", "```mmd\nA\n```", '<pre class="code-block" data-code-diagram="1">A</pre>'])
      expect(renderMarkdown(text)).not.toContain("data-code-diagram");
    expect(renderMarkdown("- item\n\n  ```mermaid\n  A\n  ```\n")).toContain("data-code-diagram");
  });
  it("keeps approved plans and question previews as code", () => {
    expect(renderMarkdownRevealed("```mermaid\nA\n```\n")).not.toContain("data-code-diagram");
  });
});
describe("reply drawings", () => {
  const setup = () => {
    const send = vi.fn();
    const controller = new DiagramController(send);
    controller.configure(true, 1);
    return { controller, send };
  };
  it("deduplicates, restores cached drawings, and copies source in place", () => {
    const { controller, send } = setup();
    const root = reply();
    controller.attach(root); controller.attach(root);
    expect(send).toHaveBeenCalledTimes(1);
    controller.receive({ id: send.mock.calls[0][0].id, svg });
    expect(codeBlockText(root.querySelector("pre")!)).toBe(source);
    expect(renderedText(root)).toBe("```mermaid\n" + source + "\n```");
    const next = reply(); controller.attach(next);
    expect(next.querySelector("img")?.src).toBe(root.querySelector("img")?.src);
    expect(send).toHaveBeenCalledTimes(1);
    expect(next.querySelector("img")?.getAttribute("width")).toBe("1000");
    expect(next.querySelector("img")?.style.minWidth).toBe("600px");
  });
  it("keeps the old drawing through a theme change", () => {
    const { controller, send } = setup(); const root = reply(); controller.attach(root);
    controller.receive({ id: send.mock.calls[0][0].id, svg });
    const old = root.querySelector("img")!.src;
    controller.configure(true, 2);
    expect(send).toHaveBeenCalledTimes(2); expect(root.querySelector("img")!.src).toBe(old);
  });
  it("theme changes defer off-screen blocks and discard old-generation answers", () => {
    const { controller, send } = setup();
    const visible = reply();
    const offscreen = reply(source + "offscreen");
    const block = offscreen.querySelector<HTMLElement>("pre")!;
    block.getBoundingClientRect = () => ({ top: 5000, bottom: 5100 }) as DOMRect;
    controller.attach(visible); controller.attach(offscreen);
    expect(send).toHaveBeenCalledTimes(1);
    const old = send.mock.calls[0][0].id;
    controller.configure(true, 2);
    expect(send).toHaveBeenCalledTimes(2);
    controller.receive({ id: old, svg });
    expect(visible.querySelector("img")).toBeNull();
    block.getBoundingClientRect = () => ({ top: 0, bottom: 100 }) as DOMRect;
    controller.attach(offscreen);
    expect(send).toHaveBeenCalledTimes(3);
  });
  it("holds arriving drawings in selection modes", () => {
    const { controller, send } = setup(); const root = reply(); controller.attach(root);
    controller.freeze(true);
    controller.receive({ id: send.mock.calls[0][0].id, svg });
    expect(root.querySelector("img")).toBeNull();
    controller.freeze(false); expect(root.querySelector("img")).not.toBeNull();
  });
  it("refuses hidden characters including NUL, large text and too many lines before sending", () => {
    const { controller, send } = setup();
    for (const text of [source + "\u0000", source + "\u202e", "a".repeat(20001), "a\n".repeat(500)]) {
      const root = reply(text); controller.attach(root);
      expect(root.querySelector(".diagram-error")?.textContent).toMatch(/^Diagram not drawn:/);
    }
    expect(send).not.toHaveBeenCalled();
  });
  it("does nothing without a host renderer", () => {
    const send = vi.fn(); const controller = new DiagramController(send); controller.attach(reply());
    expect(send).not.toHaveBeenCalled();
  });
  it.each([
    ["took longer than 4 s", "the renderer stopped"],
    ["the renderer stopped", "took longer than 4 s"],
  ])("retries queue errors, caches parse errors, and combines %s with %s", (first, second) => {
    const { controller, send } = setup();
    const bad = reply("invalid diagram"); controller.attach(bad);
    controller.receive({ id: send.mock.calls.at(-1)![0].id, error: "Parse error on line 1" });
    controller.attach(bad); expect(send).toHaveBeenCalledTimes(1);
    for (const [i, error] of ["too many diagrams waiting", first, second].entries()) {
      const root = reply(); controller.attach(root);
      controller.receive({ id: send.mock.calls.at(-1)![0].id, error, transient: true });
      expect(root.querySelector(".diagram-error")?.textContent).toContain(i === 2 ? "the renderer stopped on it twice" : error);
    }
    controller.attach(reply()); expect(send).toHaveBeenCalledTimes(4);
  });
  it("sanitizes hostile SVG, CSS and Mermaid animated edges into valid XML", () => {
    const { controller, send } = setup(); const root = reply(); controller.attach(root);
    // Mermaid 11.17.2, dist/chunks/mermaid.core/chunk-DU6HZSFF.mjs (verbatim).
    const animatedEdges = `  @keyframes edge-animation-frame {
    from {
      stroke-dashoffset: 0;
    }
  }
  @keyframes dash {
    to {
      stroke-dashoffset: 0;
    }
  }
  & .edge-animation-slow {
    stroke-dasharray: 9,5 !important;
    stroke-dashoffset: 900;
    animation: dash 50s linear infinite;
    stroke-linecap: round;
  }
  & .edge-animation-fast {
    stroke-dasharray: 9,5 !important;
    stroke-dashoffset: 900;
    animation: dash 20s linear infinite;
    stroke-linecap: round;
  }
`;
    controller.receive({ id: send.mock.calls[0][0].id, svg: `<svg xmlns="http://www.w3.org/2000/svg" onload="evil()" viewBox="0 0 100 100"><style>${animatedEdges.replace(/&/g, "&amp;")}</style><style> @import url(https://evil/x); .bad{background:image-set('https://evil/x' 1x)} .also{background:image('https://evil/x')} .ok{fill:url(#local)}</style><style>.ok{fill:url(#local)}</style><defs><pattern id="external" href="https://evil/p.svg#p"/><pattern id="local" href="#here"/><pattern id="xlink" xmlns:xlink="http://www.w3.org/1999/xlink" xlink:href="https://evil/p.svg"/></defs><path fill="url(https://evil/f.svg#a)" stroke="url(//evil/s)" filter="url(https://evil/f)" mask="url(https://evil/m)" clip-path="url(https://evil/c)" marker-start="url(https://evil/a)" marker-mid="url(https://evil/b)" marker-end="url(https://evil/c)" data-other="url(https://evil/d)" style="background:image-set('https://evil/b.png' 1x)"/><path style="background:image('https://evil/b.png')"/><path style="animation-name:dash;animation-duration:1s;fill:url(//evil/x);stroke:url(#local)"/><script>evil()</script><foreignObject/><image href="https://evil"/><feImage/><use href="https://evil"/><a href="javascript:evil()"/><animate/><animateTransform/><animateMotion/><set/></svg>` });
    const clean = decoded(root);
    expect(clean).not.toMatch(/@import|@keyframes|animation[\w-]*\s*:|evil|foreignObject|feImage|<use|<image|<a[ >]|<animate|<set/);
    expect(clean).toContain("url(#local)");
    expect(clean).toContain('href="#here"');
    expect(clean).toContain("stroke-dasharray: 9,5 !important");
    expect(clean).not.toMatch(/image-set\s*\(|image\s*\(/i);
    expect(new DOMParser().parseFromString(clean, "image/svg+xml").querySelector("parsererror")).toBeNull();
  });
  it("caps raw errors at 200 code points before revealing hidden characters", () => {
    const { controller, send } = setup(); const root = reply(); controller.attach(root);
    const error = "x".repeat(198) + "\u202e" + "😀";
    controller.receive({ id: send.mock.calls[0][0].id, error: error + "discard\nsecond" });
    expect(root.querySelector(".diagram-error")!.textContent).toBe("Diagram not drawn: " + "x".repeat(198) + "⟨U+202E⟩😀");
  });
  it("enforces drawing count, per-drawing and document byte limits", () => {
    const { controller, send } = setup();
    for (let i = 0; i < MAX_DRAWINGS; i++) {
      const root = reply(source + i); controller.attach(root); controller.receive({ id: send.mock.calls.at(-1)![0].id, svg });
    }
    const extra = reply(source + "extra"); controller.attach(extra);
    expect(extra.textContent).toContain("already shows 64 diagrams");
    const other = setup(); const large = reply(); other.controller.attach(large);
    other.controller.receive({ id: other.send.mock.calls[0][0].id, svg: "x".repeat(MAX_SVG_BYTES + 1) });
    expect(large.textContent).toContain("too large");
    const size = setup();
    const big = svg.replace("<text>A</text>", "<text>" + "a".repeat(510000) + "</text>");
    for (let i = 0; i < Math.floor(MAX_DOCUMENT_SVG_BYTES / big.length); i++) {
      const root = reply(source + i); size.controller.attach(root);
      size.controller.receive({ id: size.send.mock.calls.at(-1)![0].id, svg: big });
    }
    const full = reply(source + "full"); size.controller.attach(full);
    size.controller.receive({ id: size.send.mock.calls.at(-1)![0].id, svg: big });
    expect(full.textContent).toContain("SVG limit");
  });
});

it("releases root ownership when a reply unmounts", () => {
  const send = vi.fn(); const controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.detach(root);
  controller.receive({ id: send.mock.calls[0][0].id, svg });
  expect(root.querySelector("img")).toBeNull();
  controller.attach(root);
  expect(root.querySelector("img")).not.toBeNull();
});

it.each([true, false])("unchanged freeze does no attach work (enabled=%s), a full pass prunes once", (enabled) => {
  let checks = 0;
  const controller = new DiagramController(vi.fn(), (el) => { checks++; return el.isConnected; });
  controller.configure(enabled, 1);
  const roots = Array.from({ length: 300 }, (_, i) => reply(source + i));
  roots.forEach((root) => controller.attach(root));
  const attach = vi.spyOn(controller, "attach");
  checks = 0;
  controller.freeze(false);
  expect(attach).not.toHaveBeenCalled(); expect(checks).toBe(0);
  controller.freeze(true); controller.freeze(false);
  expect(checks).toBeLessThan(3 * roots.length);
  if (!enabled) expect(checks).toBe(0);
});

it("unobserves requested fences and only attaches intersecting roots", () => {
  let callback: IntersectionObserverCallback;
  const observe = vi.fn(), unobserve = vi.fn();
  vi.stubGlobal("IntersectionObserver", class {
    constructor(cb: IntersectionObserverCallback) { callback = cb; }
    observe = observe; unobserve = unobserve;
  });
  const send = vi.fn(), controller = new DiagramController(send);
  controller.configure(true, 1);
  const roots = [reply("A"), reply("B")];
  let visible = false;
  roots.forEach((root) => {
    root.querySelector<HTMLElement>("pre")!.getBoundingClientRect = () => ({ top: visible ? 0 : 5000, bottom: visible ? 100 : 5100 }) as DOMRect;
    controller.attach(root);
  });
  const untouched = vi.spyOn(roots[1], "querySelectorAll");
  visible = true;
  const block = roots[0].querySelector("pre")!;
  callback!([{ target: block, isIntersecting: true } as unknown as IntersectionObserverEntry], {} as IntersectionObserver);
  expect(untouched).not.toHaveBeenCalled();
  expect(unobserve).toHaveBeenCalledWith(block);
  expect(send).toHaveBeenCalledTimes(1);
});

it("caches two renderer stops and sends exactly twice across five cycles", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply();
  for (let i = 0; i < 5; i++) {
    controller.attach(root);
    controller.receive({ id: send.mock.calls.at(-1)![0].id, error: "the renderer stopped", transient: true });
  }
  expect(send).toHaveBeenCalledTimes(2);
  expect(root.textContent).toContain("Diagram not drawn: the renderer stopped on it twice");
});

it.each(["bytes", "count"])("caches %s refusals without evicting parse errors or later entries, until a drawing leaves", (limit) => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const bad = reply("parse error"), later = reply("later type"), startup = reply("startup failure");
  for (const [root, outcome] of [[bad, { error: "Parse error on line 1" }], [later, { later: true }], [startup, { error: "the diagram renderer could not start" }]] as const) {
    controller.attach(root); controller.receive({ id: send.mock.calls.at(-1)![0].id, ...outcome });
  }
  const big = limit === "bytes" ? svg.replace("A</text>", "a".repeat(510000) + "</text>") : svg;
  const roots = Array.from({ length: limit === "bytes" ? Math.floor(MAX_DOCUMENT_SVG_BYTES / big.length) : MAX_DRAWINGS }, (_, i) => reply(source + i));
  for (const root of roots) { controller.attach(root); controller.receive({ id: send.mock.calls.at(-1)![0].id, svg: big }); }
  const refused = reply("overflow"); controller.attach(refused);
  if (limit === "bytes") controller.receive({ id: send.mock.calls.at(-1)![0].id, svg: big });
  const before = send.mock.calls.length;
  for (let i = 0; i < 10; i++) {
    controller.attach(refused); controller.attach(bad); controller.attach(later); controller.attach(startup);
    if (send.mock.calls.length > before) controller.receive({ id: send.mock.calls.at(-1)![0].id, svg: big });
  }
  expect(send).toHaveBeenCalledTimes(before);
  // Even a smaller replacement must retain its count refusal until document capacity changes.
  controller.attach(refused);
  expect(refused.querySelector("img")).toBeNull();
  const repeated = reply(source + 0); controller.attach(repeated);
  expect(repeated.querySelector("img")).toBeNull();
  roots[0].remove(); controller.detach(roots[0]);
  controller.attach(repeated);
  expect(send).toHaveBeenCalledTimes(before);
  expect(repeated.querySelector("img")).not.toBeNull();
  repeated.remove(); controller.detach(repeated);
  controller.attach(bad); controller.attach(later); controller.attach(startup);
  expect(send).toHaveBeenCalledTimes(before);
  controller.attach(refused);
  expect(send).toHaveBeenCalledTimes(before + 1);
  controller.receive({ id: send.mock.calls.at(-1)![0].id, svg: big });
  expect(refused.querySelector("img")).not.toBeNull();
});

it("keeps diagram notes out of row text and VISUAL copy, including hidden-character refusals", () => {
  const controller = new DiagramController(vi.fn()); controller.configure(true, 1);
  const root = reply(source + "\u202e"); root.className = "message-list";
  controller.attach(root);
  const note = root.querySelector<HTMLElement>(".diagram-error")!;
  expect(renderedText(root)).not.toContain("Diagram not drawn:");
  expect(codeBlockText(root.querySelector("pre")!)).toBe(source + "\u202e");
  expect(isChromeNode(note.firstChild!, root)).toBe(true);
  const code = root.querySelector("code")!.firstChild!;
  const caret = { node: code, offset: 0 };
  const text = copySelectionText({ rangeCount: 0, toString() {
    expect(note.closest('[aria-hidden="true"]')).toBeNull();
    expect(isChromeNode(note.firstChild!, root)).toBe(true);
    return source;
  } } as Selection, root, { anchor: caret, cursor: caret, kind: "char", goalX: null });
  expect(text).toBe(source);
});

it.each(["char", "line"] as const)("%s region y copies fenced source between paragraphs through the browser selection", (kind) => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = document.createElement("div"); root.className = "message-list";
  const row = document.createElement("div"); row.dataset.navStop = "row";
  row.innerHTML = '<p>ABOVE-LINE text</p>\n\n' + renderMarkdown("```mermaid\n" + source + "\n```\n").trim() + '<span class="diagram-error">Diagram not drawn: a note</span>\n\n<p>BELOW-LINE text</p>';
  root.append(row); document.body.append(root);
  controller.attach(row); controller.receive({ id: send.mock.calls[0][0].id, svg });
  const block = row.querySelector("pre")!, image = block.querySelector("img")!;
  expect(image.alt).toBe("Diagram (mermaid)");
  const above = row.querySelector("p")!.firstChild!, below = row.querySelector("p:last-child")!.firstChild!;
  const selection = window.getSelection()!;
  const range = document.createRange(); range.setStart(above, 0); range.setEnd(below, below.textContent!.length);
  selection.removeAllRanges(); selection.addRange(range);
  expect(selection.getRangeAt(0).intersectsNode(image)).toBe(true);
  // jsdom's native selection has no layout: emulate only CSS-hidden code and chrome while
  // reading the actual selected fragment. Images contribute no text, as in the browser.
  let imageHiddenDuringRead = false;
  const read = vi.spyOn(selection, "toString").mockImplementation(() => {
    expect(block.querySelector("img")).toBe(image);
    imageHiddenDuringRead = image.closest<HTMLElement>(".diagram-scroll")!.style.display === "none";
    const fragment = selection.getRangeAt(0).cloneContents();
    fragment.querySelectorAll(".diagram-drawn > code").forEach((code) => code.remove());
    fragment.querySelectorAll<HTMLElement>("[style]").forEach((el) => { if (el.style.display === "none") el.remove(); });
    if (row.hasAttribute("data-visual-copying")) fragment.querySelectorAll(".diagram-error").forEach((note) => note.remove());
    return fragment.textContent ?? "";
  });
  const before = row.innerHTML;
  const text = copySelectionText(selection as unknown as SelectionLike, root, {
    anchor: { node: above, offset: kind === "line" ? 3 : 0 },
    cursor: { node: below, offset: below.textContent!.length - (kind === "line" ? 4 : 1) }, kind, goalX: null,
  });
  expect(text).toBe("ABOVE-LINE text\n\n```mermaid\nflowchart LR\nA --> B\n```\n\nBELOW-LINE text");
  expect(imageHiddenDuringRead).toBe(true);
  expect(read).toHaveBeenCalledOnce();
  expect(row.innerHTML).toBe(before);
  expect(selection.getRangeAt(0).startContainer).toBe(above);
  expect(selection.getRangeAt(0).endContainer).toBe(below);
  selection.removeAllRanges();
});

it("restores a drawing and copy chrome when the selection read throws", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); root.className = "message-list";
  controller.attach(root); controller.receive({ id: send.mock.calls[0][0].id, svg });
  const drawing = root.querySelector<HTMLElement>(".diagram-scroll")!, image = drawing.firstChild;
  drawing.style.cssText = "overflow: auto; white-space: normal;";
  const before = root.innerHTML;
  const selection = window.getSelection()!, range = document.createRange(); range.selectNodeContents(root);
  selection.removeAllRanges(); selection.addRange(range);
  vi.spyOn(selection, "toString").mockImplementation(() => { throw new Error("selection read failed"); });
  expect(() => copySelectionText(selection as unknown as SelectionLike, root, {
    anchor: { node: root, offset: 0 }, cursor: { node: root, offset: 1 }, kind: "line", goalX: null,
  })).toThrow("selection read failed");
  expect(root.innerHTML).toBe(before);
  expect(drawing.firstChild).toBe(image);
  expect(selection.getRangeAt(0).startContainer).toBe(root);
  expect(selection.getRangeAt(0).endContainer).toBe(root);
  selection.removeAllRanges();
});

it("settles disabled requests as plain code and waits for the next display hello", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.receive({ id: send.mock.calls[0][0].id, disabled: true });
  for (let i = 0; i < 10; i++) controller.attach(root);
  expect(send).toHaveBeenCalledTimes(1);
  expect(root.querySelector(".diagram-error")).toBeNull();
  controller.configureDiagramTheme(2);
  expect(send).toHaveBeenCalledTimes(1);
  controller.configure(true, 2);
  expect(send).toHaveBeenCalledTimes(2);
});

const bundleRules = new NodeURL("../../../docs/rules/build-and-command-line.md", import.meta.url);
const bundleRecord = new NodeURL("../../../docs/canonical/dated_record.md", import.meta.url);
// The exported tree omits its private documentation.
it.skipIf(!existsSync(bundleRules) && !existsSync(bundleRecord))("records the diagram bundle measurements and ceiling in the rules and their dated entry", () => {
  const rules = readFileSync(bundleRules, "utf8");
  const record = readFileSync(bundleRecord, "utf8").split("### ")
    .find((entry) => entry.startsWith("2026-11-16 (reply diagrams: attachment, copy and renderer lifecycle)"));
  expect(record).toBeDefined();
  for (const text of [rules, record]) for (const number of ["273,625", "276,210", "289,000", "4.6 %"])
    expect(text).toContain(number);
});

it("holds a disabled outcome during selection and then restores plain source until display hello", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.receive({ id: send.mock.calls[0][0].id, svg });
  controller.configureDiagramTheme(2);
  controller.freeze(true);
  controller.receive({ id: send.mock.calls[1][0].id, disabled: true });
  expect(root.querySelector("img")).not.toBeNull();
  controller.freeze(false);
  expect(root.querySelector("img, .diagram-error")).toBeNull();
  controller.attach(root); controller.configureDiagramTheme(3);
  expect(send).toHaveBeenCalledTimes(2);
  controller.configure(true, 3);
  expect(send).toHaveBeenCalledTimes(3);
});


it.each([
  '.a{list-style:"}" image-set("https://evil" 1x)}',
  '.a{content:"}" image-set("https://evil" 1x)}',
  '.a{content:";}" image-set("https://evil" 1x)}',
  '.a{content:"}"-webkit-image-set("https://evil" 1x)}',
  '.a{fill:IMAGE-SET("https://evil" 1x)}',
  String.raw`.a{fill:im\61 ge-set("https://evil" 1x)}`,
  '@import "https://evil"; .safe{fill:red}',
  '.a{.b{} fill:image("https://evil")}',
  '.a{fill:cross-fade(red,blue)}', '.a{fill:element(#a)}', '.a{fill:src("https://evil")}',
  '.a{fill:url(" https://evil ")}',
])("drops a whole stylesheet holding an outside reference: %s", (css) => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.receive({ id: send.mock.calls[0][0].id, svg: svg.replace('<text>', `<style>${css}</style><text>`) });
  const clean = new DOMParser().parseFromString(decoded(root), "image/svg+xml");
  expect(clean.querySelector("style")).toBeNull();
});

it("removes animation after a nested rule and checks raw href values", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.receive({ id: send.mock.calls[0][0].id, svg: svg.replace('<text>', '<style>.a{.b{} animation: dash 1s infinite;fill:red}</style><defs><pattern href="/*x*/#a"/></defs><text>') });
  const clean = new DOMParser().parseFromString(decoded(root), "image/svg+xml");
  expect(clean.querySelector("style")!.textContent).not.toContain("animation:");
  expect(clean.querySelector("pattern")!.hasAttribute("href")).toBe(false);
});

it("startup failures never strike or cache a source, which can subsequently draw", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply();
  for (let i = 0; i < 3; i++) {
    controller.attach(root);
    controller.receive({ id: send.mock.calls.at(-1)![0].id, error: "the renderer stopped", transient: true, ...{ startup: true } });
    expect(root.querySelector(".diagram-error")?.textContent).toBe("Diagram not drawn: the renderer stopped");
  }
  controller.attach(root);
  expect(send).toHaveBeenCalledTimes(4);
  controller.receive({ id: send.mock.calls.at(-1)![0].id, svg });
  expect(root.querySelector("img")).not.toBeNull();
});

it("mounts a thousand replies with fewer than three connectivity checks per reply", async () => {
  let checks = 0;
  const controller = new DiagramController(vi.fn(), (el) => { checks++; return el.isConnected; });
  controller.configure(true, 1);
  for (let i = 0; i < 1000; i++) controller.attach(reply(source + i));
  await Promise.resolve();
  expect(checks).toBeLessThan(3000);
});

it.each(["count", "bytes"])("redraws a rewritten reply at the %s limit after another attach in the same task", async (limit) => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const drawing = limit === "bytes" ? svg.replace("A</text>", "a".repeat(510000) + "</text>") : svg;
  const count = limit === "bytes" ? Math.floor(MAX_DOCUMENT_SVG_BYTES / drawing.length) : MAX_DRAWINGS;
  const roots = Array.from({ length: count }, () => reply());
  controller.attach(roots[0]);
  controller.receive({ id: send.mock.calls[0][0].id, svg: drawing });
  for (const root of roots.slice(1)) controller.attach(root);
  expect(roots.every((root) => root.querySelector("img") !== null)).toBe(true);
  const extra = reply(); controller.attach(extra);
  expect(extra.querySelector("img")).toBeNull();
  expect(extra.textContent).toContain(limit === "bytes" ? "SVG limit" : "already shows 64 diagrams");
  extra.remove(); controller.detach(extra);
  await Promise.resolve();

  controller.attach(roots[0]);
  const rewritten = roots.at(-1)!;
  const oldBlock = rewritten.querySelector("pre")!;
  rewritten.innerHTML = renderMarkdown("```mermaid\n" + source + "\n```\n");
  expect(oldBlock.isConnected).toBe(false);
  controller.attach(rewritten);
  expect(rewritten.querySelector(".diagram-error")).toBeNull();
  expect(rewritten.querySelector("img")?.src).toBe(roots[0].querySelector("img")!.src);
  expect(send).toHaveBeenCalledTimes(1);
});

it.each(["Parse error", "the renderer stopped"])("drops old-generation errors and strikes for %s across fifty themes", (error) => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  const internals = controller as unknown as { cache: Map<string, unknown>; failures: Map<string, unknown> };
  for (let generation = 1; generation <= 50; generation++) {
    controller.configureDiagramTheme(generation);
    controller.receive({ id: send.mock.calls.at(-1)![0].id, error, transient: error === "the renderer stopped" });
    if (error === "the renderer stopped") {
      controller.attach(root);
      controller.receive({ id: send.mock.calls.at(-1)![0].id, error, transient: true });
    }
    expect(internals.cache.size).toBeLessThanOrEqual(1);
    expect(internals.failures.size).toBeLessThanOrEqual(1);
  }
});

it("exposes the failure note to assistive technology while excluding it from copies", () => {
  const controller = new DiagramController(vi.fn()); controller.configure(true, 1);
  const root = reply(source + "\u202e"); controller.attach(root);
  const note = root.querySelector(".diagram-error")!;
  expect(note.closest('[aria-hidden="true"]')).toBeNull();
  expect(isChromeNode(note.firstChild!, root)).toBe(true);
  expect(renderedText(root)).not.toContain("Diagram not drawn");
});

it("removes href values whose raw text does not start with a fragment", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  controller.receive({ id: send.mock.calls[0][0].id, svg: svg.replace('<text>', '<defs><pattern href="/*x*/#a"/></defs><text>') });
  const clean = new DOMParser().parseFromString(decoded(root), "image/svg+xml");
  expect(clean.querySelector("pattern")!.hasAttribute("href")).toBe(false);
});
it("keeps at most 64 strike entries, evicting the oldest", () => {
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  for (let i = 0; i < 100; i++) {
    const root = reply(source + i); controller.attach(root);
    controller.receive({ id: send.mock.calls.at(-1)![0].id, error: "the renderer stopped", transient: true });
    controller.detach(root); root.remove();
  }
  const failures = (controller as unknown as { failures: Map<string, unknown> }).failures;
  expect(failures.size).toBe(64);
  expect(failures.has(`1\0${source}0`)).toBe(false);
});

it.each(["flowchart", "sequence", "class"])("keeps the themed Mermaid 11.17.2 %s stylesheet beside a rejected stylesheet", (kind) => {
  const real = readFileSync(new NodeURL(`./fixtures/diagrams/mermaid-11.17.2-${kind}.svg`, import.meta.url), "utf8");
  const send = vi.fn(), controller = new DiagramController(send); controller.configure(true, 1);
  const root = reply(); controller.attach(root);
  const hostile = '<style>.a{content:"}" image-set("https://evil" 1x)}</style>';
  controller.receive({ id: send.mock.calls[0][0].id, svg: real.replace('</svg>', hostile + '</svg>') });
  const before = new DOMParser().parseFromString(real, "image/svg+xml");
  const clean = new DOMParser().parseFromString(decoded(root), "image/svg+xml");
  expect(clean.querySelectorAll("style")).toHaveLength(before.querySelectorAll("style").length);
  expect(clean.querySelector("style")!.textContent).toContain("#123456");
  expect(clean.querySelector("style")!.textContent).toContain("#abcdef");
  expect(clean.querySelector("style")!.textContent).toContain("fill:");
  expect(decoded(root)).not.toContain("https://evil");
});
