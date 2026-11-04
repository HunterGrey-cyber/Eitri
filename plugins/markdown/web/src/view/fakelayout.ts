/* Test support, never bundled: jsdom lays nothing out. Each tagged leaf block gets 20 px per source line and a
   10 px gap, stacked in document order; 100 px more for one whose text says TALL; 1000 more for one holding an image
   whose alt text is HUGE, or text saying HOLLOW (its text is then its first row alone) or EMPTY (its text has no
   rows). A fenced code block is laid out by its code lines, one row each; a list item's own text above its nested
   list takes a row before it; the end sentinel gets 1 px. A block's descendants share its box, and its text is laid
   out in rows of 20 px filling it (a TALL paragraph wraps onto five more rows), which is what a range over the text
   reports. Scrolling moves `window.scrollY` and fires `scroll`, as a browser would. */
export interface FakeLayout { restore(): void; setScroll(y: number): void; boxOf(el: Element): { top: number; height: number } | undefined }

/** The rows of 20 px a text node's block holds, as client rects at the window position `scrollY`: what
 *  `Range.getClientRects` gives for a range over that text. */
export function fakeTextRects(box: { top: number; height: number } | undefined, scrollY: number): DOMRect[] {
  if (!box) return [];
  const out: DOMRect[] = [];
  for (let k = 0; k * 20 < box.height - 0.5; k++) {
    const top = box.top + k * 20 - scrollY;
    const height = Math.min(20, box.height - k * 20);
    out.push({ top, height, bottom: top + height, left: 0, right: 600, width: 600, x: 0, y: top, toJSON: () => ({}) } as DOMRect);
  }
  return out;
}

/* The first line (zero-based, within its `pre`) a text node holds, and how many lines it reaches into: the code's
   line breaks before it and inside it, counted once per text of the `pre`. */
const preCache = new WeakMap<Element, { text: string; starts: Map<Node, [number, number]> }>();
function preLines(pre: Element, node: Text): [number, number] {
  let known = preCache.get(pre);
  if (!known || known.text !== pre.textContent) {
    const starts = new Map<Node, [number, number]>();
    let line = 0;
    const walker = pre.ownerDocument.createTreeWalker(pre, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      const value = n.nodeValue ?? "";
      const breaks = (value.replace(/\n$/, "").match(/\n/g) ?? []).length;
      starts.set(n, [line, breaks + 1]);
      line += (value.match(/\n/g) ?? []).length;
    }
    known = { text: pre.textContent ?? "", starts };
    preCache.set(pre, known);
  }
  return known.starts.get(node) ?? [0, 1];
}

/** Makes a range over a text node report the rows of its block (`boxOf`); returns the undo. */
export function installFakeRows(boxOf: (el: Element) => { top: number; height: number } | undefined): () => void {
  const proto = Range.prototype as Range & { getClientRects?: () => DOMRect[] };
  const had = Object.getOwnPropertyDescriptor(proto, "getClientRects");
  Object.defineProperty(proto, "getClientRects", {
    configurable: true,
    value: function (this: Range) {
      const node = this.startContainer;
      const el = node.nodeType === Node.TEXT_NODE ? node.parentElement : (node as Element);
      const box = el ? boxOf(el) : undefined;
      if (!box) return [];
      // Code is one row per line: a text node reports the lines it holds.
      const pre = el?.closest("pre");
      if (pre && node.nodeType === Node.TEXT_NODE) {
        const [from, count] = preLines(pre, node as Text);
        return fakeTextRects({ top: box.top + 20 * from, height: 20 * count }, window.scrollY);
      }
      // A HOLLOW block's text is its first row; the rest of it is empty. An EMPTY block has no rows at all.
      const text = el?.closest(".code-line")?.textContent ?? "";
      if (text.includes("EMPTY")) return [];
      const hollow = text.includes("HOLLOW");
      return fakeTextRects(hollow ? { top: box.top, height: 20 } : box, window.scrollY);
    },
  });
  // A range's box: its rows together (a find match is one word, on its block's first row here).
  const hadBox = Object.getOwnPropertyDescriptor(proto, "getBoundingClientRect");
  Object.defineProperty(proto, "getBoundingClientRect", {
    configurable: true,
    value: function (this: Range) {
      const rects = (this as Range & { getClientRects(): DOMRect[] }).getClientRects();
      const top = rects[0]?.top ?? 0;
      const height = rects[0]?.height ?? 0;
      return { top, height, bottom: top + height, left: 0, right: 600, width: 600, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
    },
  });
  const put = (name: string, d: PropertyDescriptor | undefined) => {
    if (d) Object.defineProperty(proto, name, d);
    else delete (proto as unknown as Record<string, unknown>)[name];
  };
  return () => { put("getClientRects", had); put("getBoundingClientRect", hadBox); };
}

export function installFakeLayout(root: HTMLElement): FakeLayout {
  const layOut = () => {
    const pos = new Map<Element, { top: number; height: number }>();
    let y = 0;
    for (const el of root.querySelectorAll<HTMLElement>(".code-line")) {
      if (el.querySelector(".code-line")) {
        // A container's own text (a list item's line above its nested list) takes a row before its children.
        const own = [...el.childNodes].some((n) => n.nodeType === Node.TEXT_NODE && (n.nodeValue ?? "").trim() !== "");
        if (own) { pos.set(el, { top: y, height: 20 }); y += 30; }
        continue;
      }
      const line = Number(el.dataset.line);
      // A fenced block is laid out by its code lines alone.
      const code = el.tagName === "PRE" ? Number(el.getAttribute("data-eitri-fence") ?? NaN) : Number.NaN;
      const height = el.dataset.lineEnd === undefined ? 1
        : code > 0 ? code * 20
        : (Number(el.dataset.lineEnd) - line) * 20 + (el.textContent?.includes("TALL") ? 100 : 0)
          + (el.querySelector("img[alt='HUGE']") || /HOLLOW|EMPTY/.test(el.textContent ?? "") ? 1000 : 0);
      pos.set(el, { top: y, height });
      y += height + 10;
    }
    return pos;
  };
  const boxOf = (el: Element) => {
    const pos = layOut();
    for (let e: Element | null = el; e; e = e.parentElement) { const p = pos.get(e); if (p) return p; }
    return undefined;
  };
  const setScroll = (y: number) => {
    Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });
    window.dispatchEvent(new Event("scroll"));
  };
  const original = Element.prototype.getBoundingClientRect;
  const originalTo = window.scrollTo, originalBy = window.scrollBy;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const p = boxOf(this);
    const top = (p?.top ?? 0) - window.scrollY;
    return { top, height: p?.height ?? 0, bottom: top + (p?.height ?? 0), left: 0, right: 600, width: 600, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
  };
  Object.defineProperty(document.documentElement, "scrollHeight", {
    configurable: true, get: () => [...layOut().values()].reduce((m, p) => Math.max(m, p.top + p.height), 0) + window.innerHeight,
  });
  window.scrollTo = ((opts: ScrollToOptions) => setScroll(opts.top ?? 0)) as typeof window.scrollTo;
  window.scrollBy = ((opts: ScrollToOptions) => setScroll(window.scrollY + (opts.top ?? 0))) as typeof window.scrollBy;
  Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
  const unrows = installFakeRows(boxOf);
  return {
    setScroll,
    boxOf,
    restore() {
      Element.prototype.getBoundingClientRect = original; window.scrollTo = originalTo; window.scrollBy = originalBy;
      unrows();
    },
  };
}
