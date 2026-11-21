import { charLength } from "./chars";

/* Test support, never bundled: jsdom lays nothing out. Each tagged leaf block gets 20 px per source line and a
   10 px gap, stacked in document order; 100 px more for one whose text says TALL; 1000 more for one holding an image
   whose alt text is HUGE, or text saying HOLLOW (its text is then its first row alone) or EMPTY (its text has no
   rows). A fenced code block is laid out by its code lines, one row each; a list item's own text above its nested
   list takes a row before it; the end sentinel gets 1 px. A block's descendants share its box, and its text is laid
   out in rows of 20 px filling it (a TALL paragraph wraps onto five more rows), which is what a range over the text
   reports. An element's own box (`getBoundingClientRect`) is its block's, but inside code it is the lines its text
   is on, and an element around blocks (a list, the root) holds theirs. `clientWidth` is the box's width and
   `clientLeft` 0 (no border). Scrolling moves `window.scrollY` and fires `scroll`, as a browser would.
   With `{ chars: true }` a block's text is also laid out character by character (`charBoxes`), for what reads one
   character: a range over one code point reports that character's box, a range over part of one text node the rows
   its drawn characters are on (each as wide as they run), and `document.caretRangeFromPoint` answers. A range over a
   whole node, or over more than one, reports exactly what it does without it. */
export interface FakeLayout { restore(): void; setScroll(y: number): void; boxOf(el: Element): { top: number; height: number } | undefined }
export interface FakeLayoutOptions { chars?: boolean }

/** One character's box in document coordinates, as the character layout places it. */
interface CharBox { node: Text; offset: number; len: number; top: number; height: number; left: number; width: number }

const WIDE_CHAR = /[\u1100-\u115f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe30-\ufe4f\uff00-\uff60\uffe0-\uffe6]|\p{Extended_Pictographic}/u;
const NARROW_CHAR = /^[il.,:;'!|ftjr]$/;
/** A character's advance in its font size: CJK and emoji 1.25, white space 0.25, narrow letters 0.375, else 0.5. */
function charEm(ch: string): number {
  return WIDE_CHAR.test(ch) ? 1.25 : /\s/.test(ch) ? 0.25 : NARROW_CHAR.test(ch) ? 0.375 : 0.5;
}

/* The characters of the text an element lays out (`texts`, in order) in its box: left to right from x 0, onto the
   box's next 20 px row when the next character would pass 600 px (on the last row they run on). Outside code, white
   space after white space, or at the start or the end of the text, collapses to no width, as the engine collapses
   it; in code every line break starts a row and has no width. Each character is 1.25 of its font size high (at most
   the row), centred in its row. */
function charBoxes(texts: Text[], box: { top: number; height: number }, pre: boolean): CharBox[] {
  const flow = texts.map((t) => t.data).join("");
  const end = flow.trimEnd().length;
  const out: CharBox[] = [];
  let x = 0;
  let row = 0;
  let i = 0;
  for (const node of texts) {
    const parent = node.parentElement!;
    const font = parseFloat(getComputedStyle(parent).fontSize) || 16;
    for (let offset = 0; offset < node.data.length;) {
      const len = charLength(node, offset);
      const ch = node.data.slice(offset, offset + len);
      const collapsed = pre ? ch === "\n" : /\s/.test(ch) && (i === 0 || /\s/.test(flow[i - 1]) || i >= end);
      const width = collapsed ? 0 : charEm(ch) * font;
      if (!pre && width > 0 && x + width > 600 && (row + 1) * 20 < box.height - 0.5) { row++; x = 0; }
      const height = Math.min(20, 1.25 * font);
      out.push({ node, offset, len, top: box.top + row * 20 + (20 - height) / 2, height, left: x, width });
      x += width;
      if (pre && ch === "\n") { row++; x = 0; }
      offset += len;
      i += len;
    }
  }
  return out;
}

const clientRect = (c: { top: number; height: number; left: number; width: number }, scrollY: number): DOMRect => {
  const top = c.top - scrollY;
  return { top, height: c.height, bottom: top + c.height, left: c.left, right: c.left + c.width, width: c.width,
    x: c.left, y: top, toJSON: () => ({}) } as DOMRect;
};

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

/** The text an element's first (last) descendants lead down to, if they end in one. */
function deepestText(from: Node, side: "firstChild" | "lastChild"): Text | null {
  let n: Node | null = from;
  while (n && n.nodeType !== Node.TEXT_NODE) n = n[side];
  return n as Text | null;
}

/** Makes a range over a text node report the rows of its block (`boxOf`); with `charAt`, a range over one code point
 *  reports that character's box. Returns the undo. */
export function installFakeRows(boxOf: (el: Element) => { top: number; height: number } | undefined,
  charAt?: (node: Text, offset: number) => CharBox | undefined): () => void {
  const proto = Range.prototype as Range & { getClientRects?: () => DOMRect[] };
  const had = Object.getOwnPropertyDescriptor(proto, "getClientRects");
  const oneChar = (r: Range): CharBox | undefined => {
    const node = r.startContainer;
    if (!charAt || node !== r.endContainer || node.nodeType !== Node.TEXT_NODE) return undefined;
    const text = node as Text;
    if (r.endOffset - r.startOffset !== charLength(text, r.startOffset)) return undefined;
    return charAt(text, r.startOffset);
  };
  // Part of one text node the character layout holds: one rect per row of its drawn characters there.
  const partOf = (r: Range): DOMRect[] | undefined => {
    const node = r.startContainer;
    if (!charAt || node !== r.endContainer || node.nodeType !== Node.TEXT_NODE) return undefined;
    const text = node as Text;
    if (r.startOffset === 0 && r.endOffset === text.data.length) return undefined;
    const rows = new Map<number, CharBox[]>();
    for (let o = r.startOffset; o < r.endOffset; o += charLength(text, o)) {
      const c = charAt(text, o);
      if (!c) return undefined;
      if (c.width > 0) rows.set(c.top, [...(rows.get(c.top) ?? []), c]);
    }
    return [...rows.values()].map((cs) => {
      const left = Math.min(...cs.map((c) => c.left));
      const right = Math.max(...cs.map((c) => c.left + c.width));
      return clientRect({ top: cs[0].top, height: cs[0].height, left, width: right - left }, window.scrollY);
    });
  };
  Object.defineProperty(proto, "getClientRects", {
    configurable: true,
    value: function (this: Range) {
      const char = oneChar(this);
      if (char) return char.width > 0 ? [clientRect(char, window.scrollY)] : [clientRect({ ...char, width: 0 }, window.scrollY)];
      const part = partOf(this);
      if (part) return part;
      const node = this.startContainer;
      const el = node.nodeType === Node.TEXT_NODE ? node.parentElement : (node as Element);
      const box = el ? boxOf(el) : undefined;
      if (!box) return [];
      // Code is one row per line: a text node reports the lines it holds, and an element the lines from its first
      // text to its last.
      const pre = el?.closest("pre");
      if (pre && node.nodeType === Node.TEXT_NODE) {
        const [from, count] = preLines(pre, node as Text);
        return fakeTextRects({ top: box.top + 20 * from, height: 20 * count }, window.scrollY);
      }
      if (pre && this.startOffset === 0 && this.endOffset === node.childNodes.length) {
        const first = deepestText(node, "firstChild");
        const last = deepestText(node, "lastChild");
        if (!first || !last) return [];
        const [from] = preLines(pre, first);
        const [start, count] = preLines(pre, last);
        return fakeTextRects({ top: box.top + 20 * from, height: 20 * (start + count - from) }, window.scrollY);
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
      const char = oneChar(this);
      if (char) return clientRect(char, window.scrollY);
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

export function installFakeLayout(root: HTMLElement, options: FakeLayoutOptions = {}): FakeLayout {
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
  /* The element whose box holds `el` (itself or its nearest ancestor with one), and that box. */
  const ownerOf = (el: Element, pos = layOut()): [Element, { top: number; height: number }] | undefined => {
    for (let e: Element | null = el; e; e = e.parentElement) { const p = pos.get(e); if (p) return [e, p]; }
    return undefined;
  };
  const boxOf = (el: Element) => ownerOf(el)?.[1];
  /* An element's own box: inside code (below the block that owns the box) the lines from its first text to its last;
     around blocks, the blocks it holds; else its block's. */
  const elementBox = (el: Element): { top: number; height: number } | undefined => {
    const pos = layOut();
    const owner = ownerOf(el, pos);
    if (!owner) {
      const held = [...pos].filter(([e]) => el.contains(e)).map(([, b]) => b);
      if (held.length === 0) return undefined;
      const top = Math.min(...held.map((b) => b.top));
      return { top, height: Math.max(...held.map((b) => b.top + b.height)) - top };
    }
    const pre = el.closest("pre");
    if (owner[0] === el || !pre || !owner[0].contains(pre) && owner[0] !== pre) return owner[1];
    const first = deepestText(el, "firstChild");
    const last = deepestText(el, "lastChild");
    if (!first || !last) return { top: owner[1].top, height: 0 };
    const [from] = preLines(pre, first);
    const [start, count] = preLines(pre, last);
    return { top: owner[1].top + 20 * from, height: 20 * (start + count - from) };
  };
  /* The character layout of an element with a box: the text nodes it holds itself, in order. Kept while its box,
     its text and the page's style sheets stay the same, so a long code block is not laid out at every read. */
  const laidOut = new WeakMap<Element, { key: string; chars: Map<Text, CharBox[]> }>();
  const charsOf = (owner: Element, box: { top: number; height: number }, pos: Map<Element, { top: number; height: number }>) => {
    const sheets = [...document.querySelectorAll("style")].map((st) => st.textContent).join("");
    const key = `${box.top} ${box.height} ${root.style.cssText} ${sheets} ${owner.textContent}`;
    const known = laidOut.get(owner);
    if (known?.key === key) return known.chars;
    const texts: Text[] = [];
    const walker = document.createTreeWalker(owner, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode() as Text | null; n; n = walker.nextNode() as Text | null) {
      if (n.parentElement && ownerOf(n.parentElement, pos)?.[0] === owner) texts.push(n);
    }
    const chars = new Map<Text, CharBox[]>();
    for (const c of charBoxes(texts, box, owner.closest("pre") !== null)) chars.set(c.node, [...(chars.get(c.node) ?? []), c]);
    laidOut.set(owner, { key, chars });
    return chars;
  };
  /* A block that scrolls sideways (`.mdp-wide`) moves its characters left by its `scrollLeft`. */
  const shifted = (c: CharBox): CharBox => {
    const by = (c.node.parentElement?.closest(".mdp-wide") as HTMLElement | null)?.scrollLeft ?? 0;
    return by ? { ...c, left: c.left - by } : c;
  };
  const charAt = (node: Text, offset: number): CharBox | undefined => {
    const pos = layOut();
    const owner = node.parentElement ? ownerOf(node.parentElement, pos) : undefined;
    const c = owner ? charsOf(owner[0], owner[1], pos).get(node)?.find((x) => x.offset === offset) : undefined;
    return c && shifted(c);
  };
  const allChars = (): CharBox[] => {
    const pos = layOut();
    return [...pos].flatMap(([owner, box]) => [...charsOf(owner, box, pos).values()].flat().map(shifted));
  };
  /* The caret nearest (x, y) on the row under y, as the engine's hit test gives it; null outside the view. */
  const caretRangeFromPoint = (x: number, y: number): Range | null => {
    if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) return null;
    const docY = y + window.scrollY;
    const onRow = (c: CharBox) => { const top = c.top - (20 - c.height) / 2; return docY >= top && docY < top + 20; };
    const row = allChars().filter((c) => c.width > 0 && onRow(c));
    if (row.length === 0) return null;
    const dist = (c: CharBox) => (x < c.left ? c.left - x : x > c.left + c.width ? x - c.left - c.width : 0);
    const best = row.reduce((a, c) => (dist(c) < dist(a) ? c : a));
    const range = document.createRange();
    range.setStart(best.node, x < best.left + best.width / 2 ? best.offset : best.offset + best.len);
    return range;
  };
  const setScroll = (y: number) => {
    Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });
    window.dispatchEvent(new Event("scroll"));
  };
  const original = Element.prototype.getBoundingClientRect;
  const originalTo = window.scrollTo, originalBy = window.scrollBy;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const p = elementBox(this);
    const top = (p?.top ?? 0) - window.scrollY;
    return { top, height: p?.height ?? 0, bottom: top + (p?.height ?? 0), left: 0, right: 600, width: 600, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
  };
  const proto = Element.prototype;
  const client = { width: Object.getOwnPropertyDescriptor(proto, "clientWidth"), left: Object.getOwnPropertyDescriptor(proto, "clientLeft") };
  Object.defineProperty(proto, "clientWidth", { configurable: true, get(this: Element) { return this.getBoundingClientRect().width; } });
  Object.defineProperty(proto, "clientLeft", { configurable: true, get: () => 0 });
  Object.defineProperty(document.documentElement, "scrollHeight", {
    configurable: true, get: () => [...layOut().values()].reduce((m, p) => Math.max(m, p.top + p.height), 0) + window.innerHeight,
  });
  window.scrollTo = ((opts: ScrollToOptions) => setScroll(opts.top ?? 0)) as typeof window.scrollTo;
  window.scrollBy = ((opts: ScrollToOptions) => setScroll(window.scrollY + (opts.top ?? 0))) as typeof window.scrollBy;
  Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
  const unrows = installFakeRows(boxOf, options.chars ? charAt : undefined);
  // jsdom has no hit test: the property is the page's own while the fake stands.
  const doc = document as unknown as { caretRangeFromPoint?: (x: number, y: number) => Range | null };
  if (options.chars) doc.caretRangeFromPoint = caretRangeFromPoint;
  return {
    setScroll,
    boxOf,
    restore() {
      Element.prototype.getBoundingClientRect = original; window.scrollTo = originalTo; window.scrollBy = originalBy;
      for (const [name, d] of [["clientWidth", client.width], ["clientLeft", client.left]] as const) {
        if (d) Object.defineProperty(proto, name, d);
        else delete (proto as unknown as Record<string, unknown>)[name];
      }
      unrows();
      if (options.chars) delete doc.caretRangeFromPoint;
    },
  };
}
