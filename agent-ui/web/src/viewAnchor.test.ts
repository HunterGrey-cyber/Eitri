// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { anchorDrift, captureAnchor, restoreAnchor, type AnchorGeometry } from "./viewAnchor";
import { streamMarkdown } from "./markdown";

/* A fake layout that reflows: each row holds `chars` characters of text in lines of `width / 8` characters, 20px a
   line, 8px padding above and below; the list's viewport top is 0, and `scrollTop` is clamped as a browser's. */
function layout(chars: number[], width: number, height = 400) {
  const list = document.createElement("div");
  const rows = chars.map((n) => {
    const row = document.createElement("div");
    row.appendChild(document.createTextNode("x".repeat(n)));
    list.appendChild(row);
    return row;
  });
  document.body.appendChild(list);
  const view = { width, scrollTop: 0 };
  const perLine = () => Math.floor(view.width / 8);
  const heights = () => chars.map((n) => Math.ceil(n / perLine()) * 20 + 16);
  const tops = () => {
    let y = 0;
    return heights().map((h) => ((y += h), y - h));
  };
  const total = () => heights().reduce((a, b) => a + b, 0);
  Object.defineProperty(list, "clientHeight", { configurable: true, get: () => height });
  Object.defineProperty(list, "scrollHeight", { configurable: true, get: () => total() });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => view.scrollTop,
    set: (v: number) => {
      view.scrollTop = Math.max(0, Math.min(v, total() - height));
    },
  });
  const geometry: AnchorGeometry = {
    rect: (el) => {
      if (el === list) return { top: 0, bottom: height, left: 0, height };
      const i = rows.indexOf(el as HTMLDivElement);
      const top = tops()[i] - view.scrollTop;
      return { top, bottom: top + heights()[i], left: 0, height: heights()[i] };
    },
    caretAt: (_x, y) => {
      const t = tops();
      const h = heights();
      const i = t.findIndex((top, k) => y + view.scrollTop >= top && y + view.scrollTop < top + h[k]);
      if (i === -1) return null;
      const line = Math.max(0, Math.floor((y + view.scrollTop - t[i] - 8) / 20));
      return { node: rows[i].firstChild!, offset: Math.min(chars[i] - 1, line * perLine()) };
    },
    caretTop: (node, offset) => {
      const i = rows.findIndex((row) => row.firstChild === node);
      return tops()[i] + 8 + Math.floor(offset / perLine()) * 20 - view.scrollTop;
    },
  };
  return { list, rows, view, geometry, tops };
}

// Twenty long replies and a few short rows: deep in it, the text above the view is thousands of px tall.
const CONVERSATION = Array.from({ length: 24 }, (_, i) => (i % 3 === 0 ? 40 : 3000));

describe("viewAnchor: the reader's place across a width change", () => {
  it("keeps the line at the view's top inside a long reply when the list widens, and comes back exactly", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    view.scrollTop = tops()[16] + 600; // the middle of a long reply, its top above the view
    const before = view.scrollTop;
    const anchor = captureAnchor(list, rows, rows[16], geometry)!;
    expect(anchor.row).toBe(rows[16]);
    expect(anchor.text).not.toBeNull();
    const char = anchor.text!.offset;

    view.width = 1000; // zoomed to the window
    // Unanchored, the same scrollTop shows text pages further on (the jump this fixes): the reply is long gone above.
    expect(geometry.rect(rows[16]).bottom).toBeLessThan(-4000);
    restoreAnchor(list, anchor, geometry);
    const atTop = geometry.caretAt(8, 1)!;
    expect(atTop.node).toBe(rows[16].firstChild);
    // The same line: the character the reader had at the top is on the top line now.
    expect(Math.floor(atTop.offset / 125)).toBe(Math.floor(char / 125));

    view.width = 400; // unzoomed
    restoreAnchor(list, anchor, geometry);
    expect(view.scrollTop).toBe(before);
  });

  it("finds the reader's character again after the row's text was drawn anew and grew (a streaming reply's tail)", () => {
    const chars = [...CONVERSATION];
    const { list, rows, view, geometry, tops } = layout(chars, 400);
    view.scrollTop = tops()[16] + 600;
    const anchor = captureAnchor(list, rows, rows[16], geometry)!;
    const char = anchor.text!.offset;
    // The open tail is removed and drawn again from the longer text: a new text node, the old one gone.
    chars[16] += 1500;
    rows[16].replaceChildren(document.createTextNode("x".repeat(chars[16])));
    view.width = 1000;
    expect(restoreAnchor(list, anchor, geometry)).not.toBeNull();
    const atTop = geometry.caretAt(8, 1)!;
    expect(atTop.node).toBe(rows[16].firstChild);
    expect(Math.floor(atTop.offset / 125)).toBe(Math.floor(char / 125));
  });

  it("puts the cursor's row back at its height in the view when its top is on screen", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    view.scrollTop = tops()[12] - 150;
    const anchor = captureAnchor(list, rows, rows[12], geometry)!;
    expect(anchor).toMatchObject({ row: rows[12], y: 150, text: null });
    view.width = 1000;
    restoreAnchor(list, anchor, geometry);
    expect(geometry.rect(rows[12]).top).toBe(150);
    view.width = 400;
    restoreAnchor(list, anchor, geometry);
    expect(view.scrollTop).toBe(tops()[12] - 150);
  });

  it("anchors the first row on screen when the cursor's row is not", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    view.scrollTop = tops()[9] - 10;
    const anchor = captureAnchor(list, rows, rows[2], geometry)!;
    expect(anchor.row).toBe(rows[8]);
  });

  it("falls back to the same fraction of the row where the engine finds no text", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    const blind = { ...geometry, caretAt: () => null };
    view.scrollTop = tops()[16] + 900;
    const anchor = captureAnchor(list, rows, rows[16], blind)!;
    expect(anchor.text).toBeNull();
    view.width = 1000;
    restoreAnchor(list, anchor, blind);
    expect(Math.abs(anchorDrift(list, anchor, blind)!)).toBeLessThan(1);
    expect(geometry.caretAt(8, 1)!.node).toBe(rows[16].firstChild);
  });

  it("writes nothing once the anchored row is gone", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    view.scrollTop = tops()[10];
    const anchor = captureAnchor(list, rows, rows[10], geometry)!;
    rows[10].remove();
    view.width = 1000;
    const before = view.scrollTop;
    expect(restoreAnchor(list, anchor, geometry)).toBeNull();
    expect(view.scrollTop).toBe(before);
  });

  it("has no anchor with no row on screen", () => {
    const { list, rows, geometry } = layout([], 400);
    expect(captureAnchor(list, rows, undefined, geometry)).toBeNull();
  });
});

/* A row drawn from real Markdown, laid out by its text: lines of `perLine` characters of the row's text, 20px each.
   The point at a height is the character starting that line, wherever the nodes break. */
function markdownRow(html: string, perLine = 50, height = 400) {
  const list = document.createElement("div");
  const row = document.createElement("div");
  row.innerHTML = html;
  list.appendChild(row);
  document.body.appendChild(list);
  const view = { scrollTop: 0 };
  const textNodes = () => {
    const out: Text[] = [];
    const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) out.push(n as Text);
    return out;
  };
  const offsetOf = (node: Node, offset: number) => {
    let chars = 0;
    for (const n of textNodes()) {
      if (n === node) return chars + offset;
      chars += n.data.length;
    }
    return -1;
  };
  const at = (chars: number) => {
    for (const n of textNodes()) {
      if (chars < n.data.length) return { node: n as Node, offset: chars };
      chars -= n.data.length;
    }
    return null;
  };
  const rowHeight = () => Math.ceil((row.textContent ?? "").length / perLine) * 20;
  Object.defineProperty(list, "clientHeight", { configurable: true, get: () => height });
  Object.defineProperty(list, "scrollTop", { configurable: true, get: () => view.scrollTop, set: (v: number) => (view.scrollTop = v) });
  const geometry: AnchorGeometry = {
    rect: (el) => {
      if (el === list) return { top: 0, bottom: height, left: 0, height };
      return { top: -view.scrollTop, bottom: rowHeight() - view.scrollTop, left: 0, height: rowHeight() };
    },
    caretAt: (_x, y) => at(Math.floor((y + view.scrollTop) / 20) * perLine),
    caretTop: (node, offset) => {
      const chars = offsetOf(node, offset);
      return chars < 0 ? null : Math.floor(chars / perLine) * 20 - view.scrollTop;
    },
  };
  return { list, row, view, geometry, perLine };
}

describe("viewAnchor: a redraw that changes the text before the reader's point", () => {
  it("finds the reader's line again when a later reference definition turns earlier [text][r] into links", () => {
    // Sentences numbered so each line's text is unique, with a reference link in every one before the reader.
    const body = Array.from({ length: 40 }, (_, i) => `Sentence ${i} names [docs][r] once.`).join(" ");
    const first = streamMarkdown(null, body + "\n\nThe reply goes on");
    const { list, row, view, geometry, perLine } = markdownRow(first.added + first.tail);
    expect(row.textContent).toContain("[docs][r]"); // no definition yet: drawn as written
    view.scrollTop = 15 * 20; // line 15, in the middle of the sentences
    const anchor = captureAnchor(list, [row], row, geometry)!;
    const before = row.textContent!;
    // The sentence the reader's character is in: the last "Sentence N" that starts at or before it.
    const sentenceAt = (text: string, chars: number) => [...text.slice(0, chars + 1).matchAll(/Sentence (\d+)/g)].pop()![1];
    const reading = sentenceAt(before, anchor.text!.chars);
    // The definition arrives: the whole reply is drawn again and every "[docs][r]" above becomes "docs".
    const second = streamMarkdown(first, body + "\n\nThe reply goes on\n\n[r]: https://example.com/docs\n");
    row.innerHTML = second.added + second.tail;
    const after = row.textContent!;
    expect(after.length).toBeLessThan(before.length - 100);
    restoreAnchor(list, anchor, geometry);
    const top = geometry.caretAt(8, 1)!;
    const topChars = (() => {
      let chars = 0;
      const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
      for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) {
        if (n === top.node) return chars + top.offset;
        chars += (n.textContent ?? "").length;
      }
      return -1;
    })();
    // The line at the top is the one holding the reader's character: the same sentence starts on it or before it.
    expect(sentenceAt(after, topChars + perLine - 1)).toBe(reading);
  });
});

describe("viewAnchor with the pinned prompt header shown", () => {
  it("anchors the first row below the header, not one under it, and keeps its line across a width change", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    // Row 15 (a short one, 36px) shows its last 16px at the list's top; row 16, a long reply, starts 16px down.
    view.scrollTop = tops()[15] + 20;
    const plain = captureAnchor(list, rows, rows[15], geometry)!;
    expect(plain.row).toBe(rows[15]);
    expect(plain.inset).toBe(0);
    // The header is the list's sibling, over the list's top 24px: row 15 is wholly under it, row 16's top too.
    const header = document.createElement("div");
    header.className = "prompt-header";
    list.after(header);
    const covered: AnchorGeometry = {
      ...geometry,
      rect: (el) => (el === header ? { top: 0, bottom: 24, left: 0, height: 24 } : geometry.rect(el)),
    };
    try {
      const anchor = captureAnchor(list, rows, rows[15], covered)!;
      expect(anchor.row).toBe(rows[16]);
      expect(anchor.inset).toBe(24);
      // The reader's line is the first one below the header.
      expect(anchor.text).not.toBeNull();
      expect(anchor.y).toBeGreaterThanOrEqual(24);
      const y = anchor.y;

      view.width = 1000;
      restoreAnchor(list, anchor, covered);
      expect(anchorDrift(list, anchor, covered)).toBe(0);
      expect(covered.caretTop(anchor.text!.node, anchor.text!.offset)).toBe(y);
      view.width = 400;
      restoreAnchor(list, anchor, covered);
      expect(view.scrollTop).toBe(tops()[15] + 20);

      // The header going away moves nothing: heights are kept from the list's own top.
      header.remove();
      expect(anchorDrift(list, anchor, geometry)).toBe(0);
    } finally {
      header.remove();
    }
  });

  it("a row starting under the header with no text to hit-test keeps its fraction from below the header", () => {
    const { list, rows, view, geometry, tops } = layout(CONVERSATION, 400);
    const header = document.createElement("div");
    header.className = "prompt-header";
    list.after(header);
    const covered: AnchorGeometry = {
      ...geometry,
      caretAt: () => null,
      rect: (el) => (el === header ? { top: 0, bottom: 24, left: 0, height: 24 } : geometry.rect(el)),
    };
    try {
      view.scrollTop = tops()[16] + 600;
      const anchor = captureAnchor(list, rows, rows[16], covered)!;
      expect(anchor.text).toBeNull();
      expect(anchor.inset).toBe(24);
      // The point is the view's top below the header: 624px into the row.
      expect(anchor.fraction! * geometry.rect(rows[16]).height).toBeCloseTo(624, 6);
      expect(anchorDrift(list, anchor, covered)).toBeCloseTo(0, 6);
    } finally {
      header.remove();
    }
  });
});
