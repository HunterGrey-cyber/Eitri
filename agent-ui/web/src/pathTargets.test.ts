// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { pathAtCaret, pathRanges, pathSelectionOffsets, resolvePathTarget } from "./pathTargets";
import type { PathOffsets, PathTarget } from "./pathTargets";
import { pathsInText } from "./paths";

afterEach(() => { document.body.innerHTML = ""; });

function row(html: string): HTMLElement {
  const el = document.createElement("div");
  el.setAttribute("data-nav-stop", "row");
  el.setAttribute("data-row-key", "m-1");
  el.innerHTML = html;
  document.body.append(el);
  return el;
}
const ref = { path: "a/b.rs", line: null };
function target(offsets: PathOffsets, text = ref.path): PathTarget {
  return { kind: "path", rowKey: "m-1", ...offsets, text };
}
function resolve(offsets: PathOffsets | null, text = ref.path): Range | null {
  return offsets === null ? null : resolvePathTarget(document.body, target(offsets, text));
}

describe("pathRanges", () => {
  it("gives the line reference and the bare reference distinct occurrences in either ref order", () => {
    const el = row("see a.rs:12, then a.rs");
    const bare = { path: "a.rs", line: null };
    const line = { path: "a.rs", line: 12 };
    expect(pathRanges(el, [bare, line])).toEqual([{ start: 18, end: 22 }, { start: 4, end: 11 }]);
    expect(pathRanges(el, [line, bare])).toEqual([{ start: 4, end: 11 }, { start: 18, end: 22 }]);
  });
  it("never assigns the same starting offset to two references", () => {
    expect(pathRanges(row("a/b.rs a/b.rs"), [ref, ref])).toEqual([{ start: 0, end: 6 }, { start: 7, end: 13 }]);
  });
  it("finds all three paths in a sentence, ending the last offset before its dot", () => {
    const paths = ["transport/proto/frame_test.go", "internal/session/forward_test.go", "internal/session/mesh_test.go"];
    const text = `Updated ${paths[0]}, ${paths[1]} and ${paths[2]}.`;
    const ranges = pathRanges(row(text), paths.map((path) => ({ path, line: null })));
    expect(ranges.map((offsets, i) => resolve(offsets, paths[i])?.toString())).toEqual(paths);
    expect(ranges[2]?.end).toBe(text.length - 1);
  });
  it.each(["a/b.rs.", "a/b.rs. Next", "a/b.rs.,", "a/b.rs.)", "a/b.rs...", "a/b.rs... Next", "a/b.rs...),"])(
    "accepts a sentence-final dot in %s", (text) => {
      expect(pathRanges(row(text), [ref])[0]).toEqual({ start: 0, end: 6 });
    },
  );
  it.each(["see a/b.rs here", "see <code>a/b.rs</code> here", "see <span>a/</span><span>b.rs</span> here"])(
    "finds rendered text in %s", (html) => {
      const offsets = pathRanges(row(html), [ref])[0];
      expect(offsets).toEqual({ start: 4, end: 10 });
      expect(resolve(offsets)?.toString()).toBe(ref.path);
    },
  );
  it("de-duplicates the path and labels its first occurrence", () => {
    const el = row("<code>a/b.rs</code> and <code>a/b.rs</code>");
    const refs = pathsInText(el.textContent!);
    expect(refs).toEqual([ref]);
    const ranges = pathRanges(el, refs);
    expect(ranges).toEqual([{ start: 0, end: 6 }]);
    expect(resolve(ranges[0])?.startContainer).toBe(el.firstChild!.firstChild);
  });
  it.each(["x/a/b.rs", "xa/b.rs", "a/b.rs/extra", "a/b.rsx", "a/b.rs-more", "a/b.rs.old", "a/b.rs./extra", "a/b.rs.-old", "a/b.rs._old"])(
    "refuses a partial path inside %s", (text) => {
      expect(pathRanges(row(text), [ref])).toEqual([null]);
    },
  );
  it("continues past a longer path to the first exact occurrence", () => {
    const el = row("x/a/b.rs then a/b.rs and a/b.rs");
    expect(pathRanges(el, [ref])[0]?.start).toBe(14);
  });
  it("includes :line, across text nodes, and refuses a partial line number", () => {
    const el = row("a/b.rs:123 then <code>a/b.rs</code><span>:12</span>");
    const offsets = pathRanges(el, [{ ...ref, line: 12 }])[0];
    expect(offsets).toEqual({ start: 16, end: 25 });
    const range = resolve(offsets, "a/b.rs:12")!;
    expect(range.toString()).toBe("a/b.rs:12");
    expect(range.startContainer).toBe(el.querySelector("code")!.firstChild);
    expect(range.endContainer).toBe(el.querySelector("span")!.firstChild);
    expect(range.endOffset).toBe(3);
  });
  it("skips chrome and unrendered text before the drawn occurrence", () => {
    const el = row('<button>a/b.rs</button><span aria-hidden="true">a/b.rs</span><span hidden>a/b.rs</span><details><summary>fold</summary>a/b.rs</details> <code>a/b.rs</code>');
    const offsets = pathRanges(el, [ref])[0];
    expect(offsets).toEqual({ start: 5, end: 11 });
    expect(resolve(offsets)?.startContainer).toBe(el.querySelector("code")!.firstChild);
  });
  it("returns null for folded, shortened, or absent paths", () => {
    expect(pathRanges(row("<details><summary>b.rs</summary>a/b.rs</details>"), [ref, { path: "c/d.rs", line: null }])).toEqual([null, null]);
    const hidden = row("a/b.rs");
    hidden.hidden = true;
    expect(pathRanges(hidden, [ref])).toEqual([null]);
  });
  it("finds only the occurrence fully inside V-LINE's selection offsets", () => {
    const el = row("<p>a/b.rs</p>\n<p><span>a/</span><code>b.rs</code> c/d.rs</p>\n<p>a/b.rs</p>");
    const selected = document.createRange();
    selected.selectNodeContents(el.children[1]);
    const limit = pathSelectionOffsets(el, selected);
    expect(limit).toEqual({ start: 7, end: 20 });
    const offsets = pathRanges(el, [ref], limit)[0];
    selected.collapse(true);
    expect(resolve(offsets)?.startContainer).toBe(el.querySelector("span")!.firstChild);
    expect(resolve(offsets)?.toString()).toBe(ref.path);
    selected.selectNodeContents(el.children[1]);
    selected.setStart(el.querySelector("code")!.firstChild!, 1);
    expect(pathRanges(el, [ref], pathSelectionOffsets(el, selected))).toEqual([null]);
  });
  it("clips a selection to each touched row and excludes untouched rows", () => {
    const first = row("before a/b.rs");
    const second = row("a/b.rs after");
    const untouched = row("a/b.rs");
    const selected = document.createRange();
    selected.setStart(first.firstChild!, 7);
    selected.setEnd(second.firstChild!, 6);
    expect(pathSelectionOffsets(first, selected)).toEqual({ start: 7, end: 13 });
    expect(pathSelectionOffsets(second, selected)).toEqual({ start: 0, end: 6 });
    expect(pathSelectionOffsets(untouched, selected)).toBeNull();
    expect(pathRanges(untouched, [ref], null)).toEqual([null]);
  });
});

describe("resolvePathTarget", () => {
  it.each(["x", "/extra", ".old", "...more", ":12"])("refuses a path extended after capture by %s", (suffix) => {
    const el = row("see a/b.rs");
    const at = target(pathRanges(el, [ref])[0]!);
    el.append(suffix);
    expect(resolvePathTarget(document.body, at)).toBeNull();
  });
  it("refuses a new path character immediately before the captured span", () => {
    const el = row("see a/b.rs");
    const at = target(pathRanges(el, [ref])[0]!);
    el.textContent = "seexa/b.rs";
    expect(resolvePathTarget(document.body, at)).toBeNull();
  });
  it.each(["...", "...)", "... Next"])("keeps a captured path before sentence dots %s", (suffix) => {
    const el = row("see a/b.rs");
    const at = target(pathRanges(el, [ref])[0]!);
    el.append(suffix);
    expect(resolvePathTarget(document.body, at)?.toString()).toBe(ref.path);
  });
  it("resolves the same offsets into new nodes after the row is replaced", () => {
    const el = row("see <code>a/b.rs</code> here");
    const at = target(pathRanges(el, [ref])[0]!);
    const oldNode = resolvePathTarget(document.body, at)!.startContainer;
    const replacement = el.cloneNode(false) as HTMLElement;
    replacement.innerHTML = "see <span>a/</span><code>b.rs</code> here";
    el.replaceWith(replacement);
    const range = resolvePathTarget(document.body, at)!;
    expect(oldNode.isConnected).toBe(false);
    expect(range.toString()).toBe(ref.path);
    expect(range.startContainer).toBe(replacement.querySelector("span")!.firstChild);
    expect(range.endContainer).toBe(replacement.querySelector("code")!.firstChild);
    expect(range.startContainer.isConnected && range.endContainer.isConnected).toBe(true);
  });
  it("returns null if the characters at the stored offsets change", () => {
    const el = row("see a/b.rs here");
    const at = target(pathRanges(el, [ref])[0]!);
    el.textContent = "see c/d.rs here";
    expect(resolvePathTarget(document.body, at)).toBeNull();
    el.textContent = "see more a/b.rs here";
    expect(resolvePathTarget(document.body, at)).toBeNull();
  });
  it("returns null for a missing row", () => {
    row("a/b.rs");
    expect(resolvePathTarget(document.body, { ...target({ start: 0, end: 6 }), rowKey: "gone" })).toBeNull();
  });
  it("refuses a nested row impersonating the stored key", () => {
    const el = row('<div data-nav-stop="row" data-row-key="forged">a/b.rs</div>');
    expect(resolvePathTarget(document.body, { ...target({ start: 0, end: 6 }), rowKey: "forged" })).toBeNull();
    el.remove();
    expect(resolvePathTarget(document.body, target({ start: 0, end: 6 }))).toBeNull();
  });
});

describe("pathAtCaret", () => {
  it.each([
    '<span>fix src/a.rs</span><div>sent from this phone</div>',
    '<div>fix <span>src/</span><code>a.rs</code></div><div>next line</div>',
    '<div>earlier line</div><div>fix <span>src/a.rs</span></div><div>next line</div>',
    '<span>fix src/a.rs</span><br><span>next line</span>',
    '<span>fix src/a.rs</span><div></div><span>next line</span>',
    '<div>earlier line</div><span>fix src/a.rs</span><div>next line</div>',
  ])("separates block and line breaks, preserving inline spans in %s", (html) => {
    const el = row(html);
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let node: Node | null;
    while ((node = walker.nextNode()) !== null) {
      if ((node as Text).data.includes("src/")) break;
    }
    const offset = (node as Text).data.indexOf("src/") + 1;
    expect(pathAtCaret(el, node!, offset)).toEqual({ path: "src/a.rs", line: null });
  });
});
