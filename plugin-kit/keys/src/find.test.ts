// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { caseSensitive, matchRanges } from "./find";
import type { InstallOptions } from "./index";
import "./index";
import { install } from "./install";
import { GENERIC_ROWS } from "./rows";

let cleanup: (() => void) | undefined;
let y = 0;
let x = 0;
/** Each placed element's top in document coordinates; jsdom lays nothing out. */
let tops: Map<Element, number>;
let rangeRect: PropertyDescriptor | undefined;
let registry: Map<string, { ranges: Range[] }>;

function rect(top: number, height = 20, width = 100): DOMRect {
  return { x: 0, y: top, left: 0, top, right: width, bottom: top + height, width, height, toJSON: () => ({}) } as DOMRect;
}

beforeEach(() => {
  y = 0;
  x = 0;
  tops = new Map();
  // Text fixtures have boxes unless CSS removes them; jsdom performs no layout.
  vi.spyOn(Element.prototype, "getClientRects").mockImplementation(function (this: Element) {
    for (let el: Element | null = this; el; el = el.parentElement) {
      if (getComputedStyle(el).display === "none") return [] as unknown as DOMRectList;
    }
    return (getComputedStyle(this).display === "contents" ? [] : [rect(0)]) as unknown as DOMRectList;
  });
  window.eitriPlugin = { call: vi.fn(async () => ({ rows: GENERIC_ROWS })), on: () => () => {} };
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => y });
  // Reduced motion makes every scroll one instant write.
  Object.defineProperty(window, "matchMedia", { configurable: true, value: (q: string) => ({ matches: q.includes("reduce") }) });
  Object.defineProperty(window, "scrollX", { configurable: true, get: () => x });
  vi.spyOn(window, "scrollTo").mockImplementation(((opts: ScrollToOptions) => {
    if (opts.top !== undefined) y = opts.top;
    if (opts.left !== undefined) x = opts.left;
  }) as typeof window.scrollTo);
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    const top = tops.get(this);
    return top === undefined ? rect(0, 0, 0) : rect(top - y);
  });
  // A range's box is its text's, as an engine lays a one-line match out.
  rangeRect = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");
  Object.defineProperty(Range.prototype, "getBoundingClientRect", {
    configurable: true,
    value: function (this: Range) { return this.startContainer.parentElement!.getBoundingClientRect(); },
  });
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  document.body.replaceChildren();
  document.getSelection()?.removeAllRanges();
  if (rangeRect) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeRect);
  else delete (Range.prototype as unknown as Record<string, unknown>).getBoundingClientRect;
  delete (window as unknown as Record<string, unknown>).matchMedia;
  delete (globalThis as Record<string, unknown>).CSS;
  delete (globalThis as Record<string, unknown>).Highlight;
  vi.restoreAllMocks();
});

function useHighlights() {
  registry = new Map();
  (globalThis as Record<string, unknown>).CSS = { highlights: registry };
  (globalThis as Record<string, unknown>).Highlight = class { ranges: Range[]; constructor(...r: Range[]) { this.ranges = r; } };
}

/** An article whose paragraphs sit at the given tops, each saying `text`. */
function article(placed: number[], text = "alpha") {
  const root = document.createElement("article");
  for (const top of placed) {
    const p = document.createElement("p");
    p.textContent = `${text} at ${top}`;
    tops.set(p, top);
    root.append(p);
  }
  document.body.append(root);
  return root;
}

function key(value: string, init: KeyboardEventInit = {}) {
  const ev = new KeyboardEvent("keydown", { key: value, bubbles: true, cancelable: true, ...init });
  (document.activeElement ?? document.body).dispatchEvent(ev);
  return ev;
}

async function start(extra: Partial<InstallOptions> = {}) {
  cleanup = await install({ rows: [], run: () => false, initial: { timeoutlen: 1000, hasKeys: true, prefix: "Ctrl+b" }, ...extra });
}

function prompt() {
  return document.getElementById("eitri-keys-find") as HTMLFormElement | null;
}

function search(text: string, end: KeyboardEventInit & { key?: string } = { key: "Enter" }) {
  key("/");
  const input = prompt()!.querySelector("input")!;
  input.value = text;
  input.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...end }));
}

function hudMessage() {
  return document.querySelector(".eitri-keys-message")!.textContent;
}

function hudMode() {
  return (document.getElementById("eitri-keys-hud") as HTMLElement).dataset.mode;
}

describe("matching", () => {
  it("caseSensitive: smart is exact only with a capital; ignore never; match always", () => {
    expect(caseSensitive("rust", "smart")).toBe(false);
    expect(caseSensitive("Rust", "smart")).toBe(true);
    expect(caseSensitive("Rust", "ignore")).toBe(false);
    expect(caseSensitive("rust", "match")).toBe(true);
    const root = document.createElement("div");
    root.innerHTML = "<p>Rust and rust</p>";
    expect(matchRanges(root, "rust", undefined, "smart").length).toBe(2);
    expect(matchRanges(root, "Rust", undefined, "smart").length).toBe(1);
    expect(matchRanges(root, "Rust", undefined, "ignore").length).toBe(2);
    expect(matchRanges(root, "rust", undefined, "match").length).toBe(1);
  });

  it("matchRanges finds a phrase that crosses inline elements", () => {
    const root = document.createElement("div");
    root.innerHTML = "<p>foo <code>ba</code>r</p><p>foo\n   bar</p>";
    const found = matchRanges(root, "foo bar", undefined, "smart");
    expect(found.map((range) => range.toString())).toEqual(["foo bar", "foo\n   bar"]);
    expect(found[0].startContainer.textContent).toBe("foo ");
    expect(found[0].endContainer.textContent).toBe("r");
  });

  it("matchRanges leaves out the kit's own elements, script and style, hidden subtrees and the exclude selector", () => {
    const root = document.createElement("div");
    root.innerHTML = [
      "<p>alpha</p>", "<script>alpha</script>", "<style>alpha{}</style>", "<noscript>alpha</noscript>",
      "<template>alpha</template>", "<div hidden><span>alpha</span></div>", '<div aria-hidden="true">alpha</div>',
      '<div class="skip"><b>alpha</b></div>', '<div id="eitri-keys-hud">alpha</div>', '<div id="eitri-keys-sheet">alpha</div>',
      '<div id="eitri-keys-hints">alpha</div>', '<form id="eitri-keys-find">alpha</form>',
      "<details><summary>alpha</summary>alpha<p>alpha</p></details>",
    ].join("");
    document.body.append(root);
    const found = matchRanges(root, "alpha", ".skip", "smart");
    expect(found.map((range) => range.startContainer.parentElement!.tagName)).toEqual(["P", "SUMMARY"]);
  });

  it("a line break, a block's edge and a drawn but excluded element read as a space; an undrawn one as nothing", () => {
    const root = document.createElement("div");
    root.innerHTML = '<p>foo<br>bar</p><p>baz</p><p>qux</p><p>one<span class="skip">x</span>two</p>' +
      "<p>three<span hidden>x</span>four</p>";
    document.body.append(root);
    const count = (query: string) => matchRanges(root, query, ".skip", "smart").length;
    expect(count("foobar")).toBe(0);
    expect(count("foo bar")).toBe(1);
    expect(count("bazqux")).toBe(0);
    expect(count("baz qux")).toBe(1);
    expect(count("bar baz")).toBe(1);
    expect(count("onetwo")).toBe(0);
    expect(count("one two")).toBe(1);
    expect(count("threefour")).toBe(1);
    // A match that ends on a break ends where the text before the break ends.
    const [range] = matchRanges(root, "baz ", ".skip", "smart");
    expect(range.endContainer.textContent).toBe("baz");
    expect(range.endOffset).toBe(3);
    const [across] = matchRanges(root, "foo bar", ".skip", "smart");
    expect(across.startContainer.textContent).toBe("foo");
    expect(across.endContainer.textContent).toBe("bar");
  });
});

/** `checkVisibility` as an engine answers it: false without a box, so for `display: none`, anything inside it, and
 *  `display: contents` (jsdom has no such method). */
function engineVisibility() {
  Object.defineProperty(Element.prototype, "checkVisibility", {
    configurable: true,
    value: function (this: Element) {
      for (let el: Element | null = this; el; el = el.parentElement) {
        if (getComputedStyle(el).display === "none") return false;
      }
      return getComputedStyle(this).display !== "contents";
    },
  });
  return () => { delete (Element.prototype as unknown as Record<string, unknown>).checkVisibility; };
}

describe("visibility", () => {
  it("a display: contents wrapper keeps its children searchable", () => {
    const restore = engineVisibility();
    try {
      const root = document.createElement("main");
      root.innerHTML = '<div style="display: contents"><p>alpha</p>alpha</div>' +
        '<div style="display: none"><div style="display: contents"><p>alpha</p></div></div>';
      document.body.append(root);
      const found = matchRanges(root, "alpha", undefined, "smart");
      expect(found.map((range) => range.startContainer.parentElement!.tagName)).toEqual(["P", "DIV"]);
      // A root that is itself a display: contents wrapper is searched too.
      const wrapper = root.firstElementChild!;
      expect(matchRanges(wrapper, "alpha", undefined, "smart").length).toBe(2);
    } finally {
      restore();
    }
  });

  it("a root that is hidden or excluded yields no match", () => {
    const restore = engineVisibility();
    try {
      const hidden = document.createElement("main");
      hidden.hidden = true;
      hidden.textContent = "alpha";
      const plain = document.createElement("main");
      plain.className = "skip";
      plain.textContent = "alpha";
      const unshown = document.createElement("main");
      unshown.style.display = "none";
      unshown.innerHTML = "alpha <p>alpha</p>";
      document.body.append(hidden, plain, unshown);
      expect(matchRanges(hidden, "alpha", undefined, "smart")).toEqual([]);
      expect(matchRanges(plain, "alpha", ".skip", "smart")).toEqual([]);
      expect(matchRanges(plain, "alpha", undefined, "smart").length).toBe(1);
      expect(matchRanges(unshown, "alpha", undefined, "smart")).toEqual([]);
    } finally {
      restore();
    }
  });

  it("a display: contents wrapper holding only text is searched where its parent is drawn", () => {
    const restore = engineVisibility();
    try {
      const root = document.createElement("main");
      const deep = '<span style="display: contents">'.repeat(12) + "alpha" + "</span>".repeat(12);
      root.innerHTML = '<p><span style="display: contents">alpha</span></p><p>' + deep + "</p>" +
        '<div style="display: none"><span style="display: contents">alpha</span></div>';
      document.body.append(root);
      const found = matchRanges(root, "alpha", undefined, "smart");
      expect(found.map((range) => range.startContainer.parentElement!.tagName)).toEqual(["SPAN", "SPAN"]);
    } finally {
      restore();
    }
  });

  it("without checkVisibility an element's client rects decide whether its text is searched", () => {
    // jsdom has no checkVisibility, so this is the path an engine without it takes.
    expect(typeof (Element.prototype as Partial<HTMLElement>).checkVisibility).toBe("undefined");
    const root = document.createElement("main");
    root.innerHTML = '<p>alpha</p><div style="display: none"><p>alpha</p></div>' +
      '<span style="display: contents">alpha</span><p class="boxless">alpha</p>';
    document.body.append(root);
    vi.spyOn(root.querySelector(".boxless")!, "getClientRects").mockImplementation(() => [] as unknown as DOMRectList);
    const found = matchRanges(root, "alpha", undefined, "smart");
    expect(found.map((range) => range.startContainer.parentElement!.tagName)).toEqual(["P", "SPAN"]);
  });
});

describe("the prompt and n/N", () => {
  it("/ opens the prompt in insert mode; Enter goes to the first match at or below the view's top and shows [1/3]", async () => {
    article([100, 1000, 2000]);
    await start();
    key("/");
    const input = prompt()!.querySelector("input")!;
    expect(prompt()!.hidden).toBe(false);
    expect(document.activeElement).toBe(input);
    expect(hudMode()).toBe("insert");
    input.value = "alpha";
    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    input.dispatchEvent(enter);
    expect(enter.defaultPrevented).toBe(true);
    expect(prompt()!.hidden).toBe(true);
    expect(document.activeElement).not.toBe(input);
    expect(hudMode()).toBe("normal");
    expect(hudMessage()).toBe("[1/3]");
    // A third of the view above the match's top.
    expect(y).toBe(Math.max(0, Math.round(100 - window.innerHeight / 3)));

    y = 1500;
    search("alpha");
    expect(hudMessage()).toBe("[3/3]");
    expect(y).toBe(Math.round(2000 - window.innerHeight / 3));
  });

  it("n and N move and wrap with vim's messages", async () => {
    article([100, 1000, 2000]);
    await start();
    search("alpha");
    key("n");
    expect(hudMessage()).toBe("[2/3]");
    expect(y).toBe(Math.round(1000 - window.innerHeight / 3));
    key("n");
    expect(hudMessage()).toBe("[3/3]");
    key("n");
    expect(hudMessage()).toBe("search hit BOTTOM, continuing at TOP [1/3]");
    key("N", { shiftKey: true });
    expect(hudMessage()).toBe("search hit TOP, continuing at BOTTOM [3/3]");
    key("N", { shiftKey: true });
    expect(hudMessage()).toBe("[2/3]");
  });

  it("n with no search says so", async () => {
    article([100]);
    await start();
    const ev = key("n");
    expect(ev.defaultPrevented).toBe(true);
    expect(hudMessage()).toBe("no previous search");
    key("N", { shiftKey: true });
    expect(hudMessage()).toBe("no previous search");
  });

  it("a query with no match says Pattern not found and keeps the old search", async () => {
    article([100, 1000, 2000]);
    await start();
    search("alpha");
    search("beta");
    expect(hudMessage()).toBe("Pattern not found: beta");
    key("n");
    expect(hudMessage()).toBe("[2/3]");
  });

  it("Esc in the prompt closes it and restores the previous search", async () => {
    article([100, 1000, 2000]);
    const button = document.createElement("button");
    document.body.append(button);
    button.focus();
    await start();
    search("alpha");
    expect(document.activeElement).toBe(button);
    search("beta", { key: "Escape" });
    expect(prompt()!.hidden).toBe(true);
    expect(document.activeElement).toBe(button);
    expect(hudMessage()).toBe("[1/3]");
    key("n");
    expect(hudMessage()).toBe("[2/3]");
    search("beta", { key: "[", ctrlKey: true });
    expect(prompt()!.hidden).toBe(true);
    key("n");
    expect(hudMessage()).toBe("[3/3]");
  });

  it("an empty Enter searches again for the last search, as vim's / does", async () => {
    article([100, 1000, 2000]);
    await start();
    search("");
    expect(hudMessage()).toBe("no previous search");
    search("alpha");
    y = 500;
    search("");
    expect(hudMessage()).toBe("[2/3]");
  });
});

describe("highlights", () => {
  it("with CSS.highlights every match is in eitri-find and the current one in eitri-find-current", async () => {
    useHighlights();
    const root = article([100, 1000, 2000]);
    await start();
    search("alpha");
    key("n");
    expect(registry.get("eitri-find")!.ranges.map((r) => r.startContainer.parentElement)).toEqual([...root.children]);
    expect(registry.get("eitri-find-current")!.ranges.map((r) => r.startContainer.parentElement)).toEqual([root.children[1]]);
    expect(document.getSelection()!.rangeCount).toBe(0);
  });

  it("Esc in normal mode clears the highlights; n brings them back", async () => {
    useHighlights();
    article([100, 1000, 2000]);
    await start();
    search("alpha");
    expect(registry.has("eitri-find")).toBe(true);
    key("Escape");
    expect(registry.has("eitri-find")).toBe(false);
    expect(registry.has("eitri-find-current")).toBe(false);
    key("n");
    expect(registry.get("eitri-find")!.ranges.length).toBe(3);
    expect(hudMessage()).toBe("[2/3]");
  });

  it("without CSS.highlights only the current match is selected and the DOM is unchanged", async () => {
    const root = article([100, 1000, 2000]);
    const rootBefore = root.innerHTML;
    await start();
    search("alpha");
    key("n");
    const selection = document.getSelection()!;
    expect(selection.rangeCount).toBe(1);
    expect(selection.toString()).toBe("alpha");
    expect(selection.getRangeAt(0).startContainer.parentElement).toBe(root.children[1]);
    expect(root.innerHTML).toBe(rootBefore);
    expect(document.body.innerHTML).not.toContain("<mark");
    key("Escape");
    expect(selection.rangeCount).toBe(0);
  });
});

describe("a page that changes", () => {
  it("findRefresh keeps the current match near its old position after the text changed", async () => {
    useHighlights();
    const root = article([100, 1000, 2000]);
    await start();
    search("alpha");
    key("n");
    expect(hudMessage()).toBe("[2/3]");
    // The page rebuilds the paragraph the current match was in and adds a match above it.
    const rebuilt = document.createElement("p");
    rebuilt.textContent = "rebuilt alpha";
    tops.set(rebuilt, 1010);
    root.children[1].replaceWith(rebuilt);
    const added = document.createElement("p");
    added.textContent = "new alpha";
    tops.set(added, 50);
    root.prepend(added);
    window.eitriKeys.findRefresh();
    expect(registry.get("eitri-find")!.ranges.length).toBe(4);
    expect(registry.get("eitri-find-current")!.ranges[0].startContainer.parentElement).toBe(rebuilt);
    key("n");
    expect(hudMessage()).toBe("[4/4]");
  });

  it("a detached range is dropped on n instead of throwing", async () => {
    useHighlights();
    const root = article([100, 1000, 2000]);
    await start();
    search("alpha");
    key("n");
    root.children[1].remove();
    expect(() => key("n")).not.toThrow();
    expect(hudMessage()).toBe("[2/2]");
    expect(registry.get("eitri-find-current")!.ranges[0].startContainer.parentElement).toBe(root.children[1]);
    expect(registry.get("eitri-find")!.ranges.every((r) => r.startContainer.isConnected)).toBe(true);
  });

  it("n re-matches first: a match whose subtree was hidden is skipped, and a match that became visible is reached", async () => {
    useHighlights();
    const root = article([100]);
    const details = document.createElement("details");
    details.innerHTML = "<summary>notes</summary><p>alpha inside</p>";
    tops.set(details.querySelector("p")!, 500);
    root.append(details);
    const last = document.createElement("p");
    last.textContent = "alpha last";
    tops.set(last, 1000);
    root.append(last);
    await start();
    search("alpha");
    expect(hudMessage()).toBe("[1/2]");
    details.open = true;
    key("n");
    expect(hudMessage()).toBe("[2/3]");
    expect(registry.get("eitri-find-current")!.ranges[0].startContainer.parentElement!.parentElement).toBe(details);
    details.open = false;
    key("n");
    expect(hudMessage()).toBe("[2/2]");
    expect(registry.get("eitri-find-current")!.ranges[0].startContainer.parentElement).toBe(last);
  });

  it("findSeed sets the search n walks without moving or highlighting", async () => {
    useHighlights();
    article([100, 1000, 2000]);
    await start();
    y = 500;
    window.eitriKeys.message("before");
    window.eitriKeys.findSeed("alpha");
    expect(y).toBe(500);
    expect(registry.size).toBe(0);
    expect(hudMessage()).toBe("before");
    key("n");
    expect(hudMessage()).toBe("[2/3]");
    expect(registry.get("eitri-find")!.ranges.length).toBe(3);
  });
});

describe("options", () => {
  it("the root and exclude options are honoured", async () => {
    const root = article([100, 1000, 2000]);
    const aside = document.createElement("aside");
    aside.textContent = "alpha outside";
    tops.set(aside, 10);
    document.body.prepend(aside);
    const skipped = document.createElement("p");
    skipped.className = "skip";
    skipped.textContent = "alpha skipped";
    tops.set(skipped, 150);
    root.append(skipped);
    await start({ find: { root: () => root, exclude: ".skip" } });
    search("alpha");
    expect(hudMessage()).toBe("[1/3]");
    expect(document.getSelection()!.getRangeAt(0).startContainer.parentElement).toBe(root.children[0]);
  });

  it("the first match is the first one below the top of its own scroller's view, not the window's", async () => {
    // A scroller that starts 200px down the window, scrolled so its first match is clipped above its view.
    const box = document.createElement("div");
    box.style.overflowY = "auto";
    for (const [name, value] of [["scrollHeight", 2000], ["clientHeight", 400]] as const) {
      Object.defineProperty(box, name, { configurable: true, get: () => value });
    }
    tops.set(box, 200);
    for (const top of [100, 250, 500]) {
      const p = document.createElement("p");
      p.textContent = `alpha at ${top}`;
      tops.set(p, top);
      box.append(p);
    }
    document.body.append(box);
    await start();
    search("alpha");
    expect(hudMessage()).toBe("[2/3]");
    expect(document.getSelection()!.getRangeAt(0).startContainer.parentElement).toBe(box.children[1]);
    // The scroller is inside the window's view, so the window stays where it is.
    expect(y).toBe(0);
  });

  it("a match in a scroller below the window's view brings the scroller's view into the window's", async () => {
    const box = document.createElement("div");
    box.style.overflowY = "auto";
    let inner = 0;
    Object.defineProperty(box, "scrollHeight", { configurable: true, get: () => 2000 });
    Object.defineProperty(box, "clientHeight", { configurable: true, get: () => 400 });
    Object.defineProperty(box, "scrollTop", { configurable: true, get: () => inner, set: (v: number) => { inner = v; } });
    Object.defineProperty(box, "scrollTo", { configurable: true, value: (o: ScrollToOptions) => { inner = o.top!; } });
    tops.set(box, 1500);
    const p = document.createElement("p");
    p.textContent = "alpha";
    box.append(p);
    document.body.append(box);
    // The paragraph sits 800px down the box's content, so both the box's scroll and the window's move it.
    vi.mocked(Element.prototype.getBoundingClientRect).mockImplementation(function (this: Element) {
      if (this === p) return rect(1500 + 800 - inner - y);
      const top = tops.get(this);
      return top === undefined ? rect(0, 0, 0) : rect(top - y);
    });
    await start();
    search("alpha");
    expect(hudMessage()).toBe("[1/1]");
    // A third of the box's view down the box, and the box scrolled into the window so the match is a third down it.
    expect(inner).toBe(Math.round(800 - 400 / 3));
    expect(Math.abs(p.getBoundingClientRect().top - window.innerHeight / 3)).toBeLessThanOrEqual(1);
  });

  it("the page's labels replace the messages", async () => {
    article([100, 1000]);
    await start({
      labels: {
        messages: {
          "find.none": "没有搜索", "find.count": "第 {index} 个，共 {total} 个", "find.notFound": "找不到：{query}",
          "find.wrapBottom": "到底了，从头开始",
        },
      },
    });
    key("n");
    expect(hudMessage()).toBe("没有搜索");
    search("beta");
    expect(hudMessage()).toBe("找不到：beta");
    search("alpha");
    expect(hudMessage()).toBe("第 1 个，共 2 个");
    key("n");
    key("n");
    expect(hudMessage()).toBe("到底了，从头开始 第 1 个，共 2 个");
    key("N", { shiftKey: true });
    expect(hudMessage()).toBe("search hit TOP, continuing at BOTTOM 第 2 个，共 2 个");
  });

  it("removing the installation forgets the search and the prompt", async () => {
    useHighlights();
    article([100, 1000]);
    await start();
    search("alpha");
    cleanup!();
    cleanup = undefined;
    expect(prompt()).toBeNull();
    expect(registry.size).toBe(0);
    await start();
    key("n");
    expect(hudMessage()).toBe("no previous search");
  });
});

describe("revealing a match sideways", () => {
  function box(width: number, clientWidth: number, scrollLeft = 0) {
    const el = document.createElement("div");
    el.style.overflowX = "auto";
    let left = scrollLeft;
    Object.defineProperty(el, "scrollWidth", { configurable: true, get: () => width });
    Object.defineProperty(el, "clientWidth", { configurable: true, get: () => clientWidth });
    Object.defineProperty(el, "scrollLeft", { configurable: true, get: () => left, set: (v: number) => { left = v; } });
    const scrollTo = vi.fn((o: ScrollToOptions) => { if (o.left !== undefined) left = o.left; });
    Object.defineProperty(el, "scrollTo", { configurable: true, value: scrollTo });
    return { el, scrollTo };
  }
  const sized = (left: number, width = 100) =>
    ({ x: left, y: 0, left, top: 0, right: left + width, bottom: 20, width, height: 20, toJSON: () => ({}) }) as DOMRect;
  /** Lays every placed element out by `place`: its left edge in window coordinates, given the scrolls now. */
  function layout(place: Map<Element, () => number>, width = 100) {
    vi.mocked(Element.prototype.getBoundingClientRect).mockImplementation(function (this: Element) {
      const left = place.get(this);
      return left === undefined ? rect(0, 0, 0) : sized(left(), width);
    });
  }

  it("a match beyond the right edge of a non-wrapping block is scrolled in, with a margin", async () => {
    const pre = box(2000, 400);
    const p = document.createElement("p");
    p.textContent = "alpha";
    pre.el.append(p);
    document.body.append(pre.el);
    layout(new Map([[pre.el, () => 50], [p, () => 50 + 1500 - pre.el.scrollLeft]]));
    await start();
    search("alpha");
    expect(hudMessage()).toBe("[1/1]");
    // The match's right edge ends 24px inside the view's right edge.
    expect(pre.el.scrollLeft).toBe(1500 + 100 - 400 + 24);
    expect(pre.scrollTo).toHaveBeenCalledWith({ left: 1224, behavior: "instant" });
  });

  it("a match already in view leaves the block where it is", async () => {
    const pre = box(2000, 400);
    const p = document.createElement("p");
    p.textContent = "alpha";
    pre.el.append(p);
    document.body.append(pre.el);
    layout(new Map([[pre.el, () => 50], [p, () => 50 + 100 - pre.el.scrollLeft]]));
    await start();
    search("alpha");
    expect(pre.scrollTo).not.toHaveBeenCalled();
    expect(pre.el.scrollLeft).toBe(0);
  });

  it("a match clipped on the left is scrolled back in, and n reaches a second match the same way", async () => {
    const pre = box(2000, 400, 1500);
    const a = document.createElement("p");
    const b = document.createElement("p");
    a.textContent = "alpha one";
    b.textContent = "alpha two";
    pre.el.append(a, b);
    document.body.append(pre.el);
    layout(new Map([[pre.el, () => 50], [a, () => 50 + 100 - pre.el.scrollLeft], [b, () => 50 + 1700 - pre.el.scrollLeft]]));
    await start();
    search("alpha");
    expect(hudMessage()).toBe("[1/2]");
    expect(pre.el.scrollLeft).toBe(100 - 24);
    key("n");
    expect(hudMessage()).toBe("[2/2]");
    expect(pre.el.scrollLeft).toBe(1700 + 100 - 400 + 24);
    key("N", { shiftKey: true });
    expect(pre.el.scrollLeft).toBe(100 - 24);
  });

  it("an outer scroller moves only when the match is still outside its view after the inner one moved", async () => {
    // The inner block sits at the outer one's left edge, and the match is inside the inner block's view once it
    // scrolls: the outer view needs no move. A second, farther inner block pokes out of the outer view.
    const outer = box(3000, 600);
    const inner = box(2000, 400);
    const p = document.createElement("p");
    p.textContent = "alpha";
    inner.el.append(p);
    outer.el.append(inner.el);
    document.body.append(outer.el);
    layout(new Map([
      [outer.el, () => 0],
      [inner.el, () => 100 - outer.el.scrollLeft],
      [p, () => 100 - outer.el.scrollLeft + 1500 - inner.el.scrollLeft],
    ]));
    await start();
    search("alpha");
    expect(inner.el.scrollLeft).toBe(1224);
    expect(outer.scrollTo).not.toHaveBeenCalled();

    // Moved so that the inner view is cut off by the outer's right edge: the outer one scrolls too.
    inner.el.scrollLeft = 0;
    outer.el.scrollLeft = 0;
    layout(new Map([
      [outer.el, () => 0],
      [inner.el, () => 500 - outer.el.scrollLeft],
      [p, () => 500 - outer.el.scrollLeft + 1500 - inner.el.scrollLeft],
    ]));
    search("alpha");
    // The inner move leaves the match 500 + 400 - 124 = 776 from the outer's left, past its 600px view.
    expect(inner.el.scrollLeft).toBe(1224);
    expect(outer.el.scrollLeft).toBe(776 + 100 - 600 + 24);
  });

  it("the window scrolls sideways when the match is beyond its view", async () => {
    Object.defineProperty(document.documentElement, "scrollWidth", { configurable: true, value: 4000 });
    Object.defineProperty(document.documentElement, "clientWidth", { configurable: true, value: 1000 });
    const p = document.createElement("p");
    p.textContent = "alpha";
    document.body.append(p);
    layout(new Map([[p, () => 2500 - x]]));
    await start();
    search("alpha");
    expect(x).toBe(2500 + 100 - 1000 + 24);
    delete (document.documentElement as unknown as Record<string, unknown>).scrollWidth;
    delete (document.documentElement as unknown as Record<string, unknown>).clientWidth;
  });
});
