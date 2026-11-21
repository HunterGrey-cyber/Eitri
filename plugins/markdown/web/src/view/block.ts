import { kit } from "../kit";
import { charLength, charsBetween, charStart, hasRtl, lowerIndex, oddsOf } from "./chars";
import { ATOM, SKIP } from "./rows";
import { eachTextAt, textSpan } from "./text-from";

/** A place on one character: a text node and the offset of the character's first code unit. */
interface Caret { node: Text; offset: number }
/** The Normal-mode moves the block makes: `h` `l` `0` `$` here, `w` `b` `e` by the keyboard kit's walker. */
export type BlockMotion = "h" | "l" | "w" | "b" | "e" | "0" | "$";
interface Span { top: number; bottom: number }
/** A drawn character of a text node: its offset and box. */
interface Drawn { offset: number; r: DOMRect }
/** A text node's part of a row: the offsets of its first and its last drawn character on the band. */
interface Seg { t: Text; head: number; tail: number }
/** A band's row as it was read: its text nodes' parts in order, and what has been counted on it since. Kept while the
 *  band object is (a band lasts while the cursor and the layout stay) and no text under the root changes, so every
 *  key on the row reads it once. */
interface Row {
  band: Span;
  layout: object | null;
  scope: Element;
  segs: Seg[];
  /** Each text node's place in `segs`. */
  at: Map<Text, number>;
  /** How many drawn characters come before each seg, as far as they have been counted. */
  before: number[];
  /** Whether each character that may draw nothing (`chars.ts::oddsOf`) does, as far as one has been asked. */
  drawn: Map<Text, Map<number, boolean>>;
  rtl: boolean | null;
}

/** Text the block never sits on: what the page does not show as text, and what is read as one box (a formula, an
 *  image, a diagram's labels). A row of nothing else has the band alone. */
const NOT_ON = `${SKIP}, ${ATOM}`;
/** The attributes whose change can take text on or off a row while no text node changes and the root keeps its size:
 *  a disclosure opened or closed, an element hidden or shown, a class or an inline style set. */
const LAYOUT_ATTRS = ["open", "hidden", "class", "style"];
/** The narrowest block, in the character's font size: a narrow letter (i, l, r) still reads as a block and not as
 *  an insert-mode bar; a wider glyph (CJK) keeps its own width. */
const MIN_WIDTH_EM = 0.6;
/** What a block that scrolls sideways (`zh` / `zl`) clips: the block hides when its character is scrolled out. */
const SCROLLER = ".mdp-wide";
/** The elements `h` and `l` keep to when no source-mapped block holds the text. */
const BLOCK_TAGS = "p, li, h1, h2, h3, h4, h5, h6, td, th, dd, dt, summary, pre, blockquote, figcaption, div";

/** The blink times of the Normal-mode `guicursor` (milliseconds): shown for `wait`, then hidden for `off` and shown for
 *  `on`, over and over. */
interface Blink { wait: number; on: number; off: number }
const BLINK_VARS = ["--nv-cursor-blinkwait-ms", "--nv-cursor-blinkon-ms", "--nv-cursor-blinkoff-ms"] as const;
/** On the block while its blink has it hidden. */
const BLINK_HIDDEN = "mdp-blink-hidden";

/** The blink the theme and the reader's settings ask for, or null for a steady block: any of the three times missing or
 *  zero (the default), or the system asking for reduced motion, which a block that flashes does not answer. */
function readBlink(): Blink | null {
  if (typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches) return null;
  const style = getComputedStyle(document.documentElement);
  const [wait, on, off] = BLINK_VARS.map((name) => {
    const ms = /^(\d{1,5})ms$/.exec(style.getPropertyValue(name).trim());
    return ms ? Number(ms[1]) : 0;
  });
  return wait > 0 && on > 0 && off > 0 ? { wait, on, off } : null;
}

/** From `o` in `data`, the first offset going `dir` that is not a line break (`o` itself when it is not one); past
 *  the end (or -1) when all are. Pure string work: a code block's line breaks draw nothing, so none is read. */
function pastBreaks(data: string, o: number, dir: 1 | -1): number {
  while (o >= 0 && o < data.length && data.charCodeAt(o) === 10) o += dir;
  return o;
}

/**
 * The block cursor: one character of the cursor's band drawn as a block in the editor cursor's colours, with the
 * character again on top in the cursor's text colour. It is one absolutely placed element over the page, as the band
 * and the margin bar are, and never changes the document.
 *
 * It is kept as its caret and that caret's index among the characters of the band's row, so a render that replaces
 * the caret's text node, or a layout that moves it off the band, puts it back on the same index of the band's row,
 * clamped to the row's last character.
 */
export class BlockCursor {
  readonly el: HTMLElement;
  private readonly glyph: HTMLElement;
  private caret: Caret | null = null;
  /** The caret's index among the drawn characters of its row; null until it is first placed on a row. */
  private at: number | null = null;
  /** The band and caret `at` was counted for: a band is the same object until the cursor or the layout moves. */
  private counted: { band: object; node: Text; offset: number } | null = null;
  /** The last row read; dropped when the text under the root changes (`changes`). */
  private rowRead: Row | null = null;
  /** Watches the root for text nodes added, removed or changed, which a row read before holds (a render's morph
   *  replaces them without the band or the layout object changing at once), and for `LAYOUT_ATTRS` (a `details`
   *  closed in a table cell folds text off a row whose box stays as it was). */
  private readonly changes: MutationObserver;
  /** The band the caret sits on while the block is placed, else null: the row `0` and `$` read. */
  private rowBand: Span | null = null;
  /** Vim's curswant: the window x `j` `k` and the page moves keep. null until a vertical move reads it from the
   *  caret; Infinity after `$`. Every horizontal move, jump and click clears it. */
  private want: number | null = null;
  /** The blink running and what it was started for: it restarts when the block moves to another character or the
   *  times change, and not on a draw that leaves both as they were. Null while the block is steady or hidden. */
  private blinking: (Blink & { node: Text; offset: number }) | null = null;
  private blinkTimer: ReturnType<typeof setTimeout> | undefined;

  /** `layout` names the page's current layout (an object that changes when the layout does): a row read for a band
   *  is reused for another band object with the same extent in the same layout. */
  constructor(private readonly root: HTMLElement, private readonly layout: () => object | null = () => null) {
    this.el = document.createElement("div");
    this.el.className = "mdp-cursor-block";
    this.el.setAttribute("aria-hidden", "true");
    this.el.hidden = true;
    this.glyph = document.createElement("span");
    this.glyph.className = "mdp-cursor-glyph";
    this.el.append(this.glyph);
    document.body.append(this.el);
    this.changes = new MutationObserver(() => { this.rowRead = null; });
    this.changes.observe(root, {
      childList: true, characterData: true, subtree: true, attributes: true, attributeFilter: LAYOUT_ATTRS,
    });
  }

  remove(): void { this.stopBlink(); this.el.remove(); this.changes.disconnect(); this.rowRead = null; }

  /** Hides the block, and with it the blink: nothing is scheduled while the block is away. */
  hide(): void { this.el.hidden = true; this.stopBlink(); }

  /** Forgets the caret and the column: the next draw puts the block on the band's first character that is not
   *  blank (a jump's landing). */
  reset(): void { this.caret = null; this.at = null; this.counted = null; this.want = null; this.rowRead = null; }

  /** The block to the character at `range`'s start, or the first one drawn after it. */
  placeAt(range: Range): void {
    this.caret = this.caretFrom(range.startContainer, range.startOffset);
    this.counted = null;
    this.want = null;
  }

  /** The column the next vertical move keeps (window x), read from the caret's middle when none is held; null with
   *  the block not on a row. */
  column(): number | null {
    if (this.want === null && this.rowBand && this.valid(this.caret)) {
      const r = this.rectOf(this.caret);
      if (r) this.want = (r.left + r.right) / 2;
    }
    return this.want;
  }

  /** The next vertical move starts from the caret's own column again (a click, a horizontal placement). */
  clearColumn(): void { this.want = null; }

  /** The caret's box in window coordinates, or null without a drawn caret. */
  caretBox(): DOMRect | null { return this.valid(this.caret) ? this.rectOf(this.caret) : null; }

  /** The block to the character of `band`'s row (document coordinates) that holds window x `x`, else the row's
   *  last one: found from the characters' own boxes, by halving over each text node's characters, so a row that is
   *  off screen (a glide still running) answers as well as one in view. `start` is the row's first text, as
   *  `Preview` reads it. False, the caret left alone, for a row with no character (an image, a rule). */
  placeNear(band: Span, x: number, start: Range | null): boolean {
    const first = start ? this.caretFrom(start.startContainer, start.startOffset) : null;
    if (!first || !this.inBand(first, band)) return false;
    const c = this.charAtX(first.node, band, x);
    if (!c) return false;
    this.caret = c;
    this.counted = null;
    return true;
  }

  /** One Normal-mode move of the block's character; true when it moved. `h` `l` stay inside the block element (its
   *  wrapped rows, not past a code line's end); `0` `$` go to the first and the last character of the band's row; `w`
   *  `b` `e` are the kit's walker, which is what visual mode uses. A move clears the column (`$` holds the row's
   *  end, and a `0` that has nowhere to go still ends it). */
  move(motion: BlockMotion): boolean {
    const c = this.caret;
    const band = this.rowBand;
    if (!band || !this.valid(c)) return false;
    let next: Caret | null;
    if (motion === "h" || motion === "l") next = this.stepLine(c, motion === "l" ? 1 : -1);
    else if (motion === "0" || motion === "$") next = this.edge(c, band, motion === "$" ? 1 : -1);
    else next = this.byKit(c, motion);
    if (motion === "$") this.want = Number.POSITIVE_INFINITY;
    else if (motion === "0") this.want = null;
    if (!next || (next.node === c.node && next.offset === c.offset)) return false;
    this.counted = null;
    this.caret = next;
    if (motion !== "$") this.want = null;
    return true;
  }

  /** The block to visual mode's cursor character, from the selection: the one before the focus when the focus comes
   *  after the anchor (the focus is the end of the selected text), else the one at the focus; the last drawn one for a
   *  focus past all the text. False, nothing changed, for a selection that is not on the page. */
  placeAtFocus(s: Selection): boolean {
    const { anchorNode, focusNode } = s;
    if (!anchorNode || !focusNode || !this.root.contains(anchorNode) || !this.root.contains(focusNode)) return false;
    const from = document.createRange();
    from.setStart(anchorNode, s.anchorOffset);
    const forward = from.comparePoint(focusNode, s.focusOffset) > 0;
    const at = this.textCaret(focusNode, s.focusOffset);
    const c = !at ? this.lastDrawn() : forward ? this.stepDrawn(at, -1) : this.drawnFrom(at);
    if (!c) return false;
    this.caret = c;
    this.counted = null;
    this.want = null;
    return true;
  }

  /** The block to visual mode's cursor as the kit gives it (`at`, a place as `motion` answers: just before the
   *  cursor's character, or a line's end, which is the line's last drawn character). A line's end with no drawn
   *  character on its own line (after an image on its own line, on a blank code line) leaves no caret, so the next
   *  draw shows the band alone there, as on an image's row. False, nothing changed, for a place that is not on the
   *  page. */
  placeAtCursor(at: { node: Node; offset: number }): boolean {
    if (!this.root.contains(at.node)) return false;
    const c = this.landing(at);
    if (!c) {
      this.reset();
      return true;
    }
    this.caret = c;
    this.counted = null;
    this.want = null;
    return true;
  }

  /** A collapsed range before the block's character while the block shows, else null. */
  caretRange(): Range | null {
    if (this.el.hidden || !this.valid(this.caret)) return null;
    const r = document.createRange();
    r.setStart(this.caret.node, this.caret.offset);
    return r;
  }

  /** The block's index among the drawn characters of its row, once it has been on one. */
  index(): number | null { return this.at; }

  /** Draws the block on its character of `band` (document coordinates). A caret no longer on the band (its text
   *  replaced, the layout moved) goes to its index on the band's row; one never placed goes to `fallback`, the row's
   *  first character that is not blank. No band, or a row with no character (an image, a rule), hides it. */
  draw(band: { top: number; bottom: number } | null, fallback: () => Range | null): void {
    this.el.hidden = true;
    const c = band ? this.place(band, fallback) : null;
    this.rowBand = c && band ? band : null;
    if (c && band) this.paint(c, band);
    if (this.el.hidden || !c) this.stopBlink();
    else this.tune(c);
  }

  /* The blink for the character `c` the block shows on, from the theme's times as they are now. A block already
     blinking for this character with these times is left to run; a move to another character, or new times, start
     the wait again, as nvim restarts it at every move. */
  private tune(c: Caret): void {
    const b = readBlink();
    const k = this.blinking;
    if (k && b && k.node === c.node && k.offset === c.offset && k.wait === b.wait && k.on === b.on && k.off === b.off) return;
    this.stopBlink();
    if (!b) return;
    this.blinking = { ...b, node: c.node, offset: c.offset };
    this.after(b.wait, true, b);
  }

  /** In `ms` the block goes hidden (`hide`) or shown, and the other change is scheduled after it; a blink no longer
   *  asked for (the system now asks for reduced motion, a time went to zero) stops there with the block shown. */
  private after(ms: number, hide: boolean, b: Blink): void {
    this.blinkTimer = setTimeout(() => {
      if (!readBlink()) { this.stopBlink(); return; }
      this.el.classList.toggle(BLINK_HIDDEN, hide);
      this.after(hide ? b.off : b.on, !hide, b);
    }, ms);
  }

  private stopBlink(): void {
    clearTimeout(this.blinkTimer);
    this.blinkTimer = undefined;
    this.blinking = null;
    this.el.classList.remove(BLINK_HIDDEN);
  }

  /* Where the block goes on `band`. A caret chosen since the last draw (a move, a find, a click, the column `j` keeps)
     that is on the band stays, and its index is counted again. A caret that is as it was, on a band the layout made
     again, and one that is gone or off the band (a render replaced its text, the row re-wrapped), go to the saved index
     of the row, clamped to its last character, even where the caret's own text node survived the render. */
  private place(band: { top: number; bottom: number }, fallback: () => Range | null): Caret | null {
    const k = this.counted;
    const c = this.caret;
    const unmoved = k !== null && c !== null && k.node === c.node && k.offset === c.offset;
    if (unmoved && k.band === band && this.valid(c)) return c;
    if (!unmoved && c && this.valid(c) && this.inBand(c, band)) return this.settle(c, band);
    const start = fallback();
    const blank = start ? this.caretFrom(start.startContainer, start.startOffset) : null;
    if (!blank || !this.inBand(blank, band)) return null;
    const saved = this.at === null ? null : this.nth(this.row(blank.node, band), this.at);
    return this.settle(saved ?? blank, band);
  }

  private settle(c: Caret, band: Span): Caret {
    this.caret = c;
    this.at = this.indexOf(c, band) ?? this.at ?? 0;
    this.counted = { band, node: c.node, offset: c.offset };
    return c;
  }

  /** The row's first (-1) or last (1) drawn character. */
  private edge(c: Caret, band: Span, dir: 1 | -1): Caret {
    const segs = this.row(c.node, band).segs;
    const s = segs[dir > 0 ? segs.length - 1 : 0];
    return s ? { node: s.t, offset: dir > 0 ? s.tail : s.head } : c;
  }

  /** The `k`th drawn character of the row, or its last one: the text nodes before it passed by their counts. */
  private nth(row: Row, k: number): Caret | null {
    for (let i = 0; i < row.segs.length; i++) {
      const s = row.segs[i];
      const n = this.before(row, i + 1) - this.before(row, i);
      if (k >= n) { k -= n; continue; }
      const code = this.inCode(s.t);
      for (let o = s.head; o <= s.tail; o += charLength(s.t, o)) {
        if (this.drawnAt(row, s.t, o, code) && k-- === 0) return { node: s.t, offset: o };
      }
    }
    const last = row.segs[row.segs.length - 1];
    return last ? { node: last.t, offset: last.tail } : null;
  }

  /** How many drawn characters of the row lie before `c`; null when `c` is not on it. */
  private indexOf(c: Caret, band: Span): number | null {
    const row = this.row(c.node, band);
    const i = row.at.get(c.node);
    const s = i === undefined ? undefined : row.segs[i];
    if (i === undefined || !s || c.offset < s.head || c.offset > s.tail) return null;
    return this.before(row, i) + this.drawnIn(row, s.t, s.head, c.offset);
  }

  /** How many drawn characters of the row come before its `i`th text node, counted once per row. */
  private before(row: Row, i: number): number {
    if (row.before.length === 0) row.before.push(0);
    for (let j = row.before.length; j <= i; j++) {
      const s = row.segs[j - 1];
      row.before.push(row.before[j - 1] + this.drawnIn(row, s.t, s.head, s.tail + charLength(s.t, s.tail)));
    }
    return row.before[i];
  }

  /** How many characters starting in [`a`, `b`) of `t` draw something: the characters, counted by halving
   *  (`chars.ts::charsBetween`), less those of them that may draw nothing (`drawnAt`) and do not. Only those few are
   *  looked at, so the count costs the same wherever on a long code line it ends. */
  private drawnIn(row: Row, t: Text, a: number, b: number): number {
    let n = charsBetween(t, a, b);
    const code = this.inCode(t);
    const odds = oddsOf(t);
    for (const list of code ? [odds.other] : [odds.blank, odds.other]) {
      for (let k = lowerIndex(list, a); k < list.length && list[k] < b; k++) if (!this.drawnAt(row, t, list[k], code)) n--;
    }
    return n;
  }

  private inCode(t: Text): boolean { return t.parentElement?.closest("pre") != null; }

  /** Whether the character at `o` draws something. A letter, digit or symbol surely does, and so does a space or tab in
   *  code, with no read of the layout; white space elsewhere may have collapsed, and a zero-width or control
   *  character has no box, so those are asked, once per row. */
  private drawnAt(row: Row, t: Text, o: number, code: boolean): boolean {
    const cp = t.data.codePointAt(o) ?? 0;
    const blank = cp === 0x20 || cp === 0x09;
    if (blank ? code : !/[\s\p{C}]/u.test(String.fromCodePoint(cp))) return true;
    let known = row.drawn.get(t);
    if (!known) row.drawn.set(t, (known = new Map()));
    let d = known.get(o);
    if (d === undefined) known.set(o, (d = this.rectOf({ node: t, offset: o }) !== null));
    return d;
  }

  /* The block's box: as wide as the glyph or `MIN_WIDTH_EM` of its font size, centred on it and cut to the box of the
     block it scrolls in; as tall as the character's line box (its own box when the line box has no height, as a
     superscript's has none), within the band. Inside, the character again with the same font, on the text's baseline:
     a line box exactly as tall as the glyph's own box puts the baseline where the text has it. Copied as text: the
     character is the document's. */
  private paint(c: Caret, band: { top: number; bottom: number }): void {
    const r = this.rectOf(c);
    const parent = c.node.parentElement;
    if (!r || !parent) return;
    const clip = this.clipBox(parent);
    if (clip && (r.left < clip.left - 0.5 || r.right > clip.right + 0.5)) return;
    const cs = getComputedStyle(parent);
    const width = Math.max(r.width, MIN_WIDTH_EM * (parseFloat(cs.fontSize) || r.height));
    const left = Math.max(r.left - (width - r.width) / 2, clip?.left ?? -Infinity);
    const right = Math.min(r.left + (width + r.width) / 2, clip?.right ?? Infinity);
    const line = parseFloat(cs.lineHeight);
    const high = Number.isFinite(line) ? (line > 0 ? line : r.height) : band.bottom - band.top;
    const mid = (r.top + r.bottom) / 2 + window.scrollY;
    const top = Math.max(band.top, mid - high / 2);
    const bottom = Math.min(band.bottom, mid + high / 2);
    Object.assign(this.el.style, {
      left: `${left + window.scrollX}px`, top: `${top}px`, width: `${right - left}px`, height: `${bottom - top}px`,
    });
    this.glyph.textContent = c.node.data.slice(c.offset, c.offset + charLength(c.node, c.offset));
    Object.assign(this.glyph.style, {
      left: `${r.left - left}px`, top: `${r.top + window.scrollY - top}px`, height: `${r.height}px`, lineHeight: `${r.height}px`,
      fontFamily: cs.fontFamily, fontSize: cs.fontSize, fontWeight: cs.fontWeight, fontStyle: cs.fontStyle,
      fontStretch: cs.fontStretch, fontVariant: cs.fontVariant, fontFeatureSettings: cs.fontFeatureSettings,
      fontVariationSettings: cs.fontVariationSettings,
    });
    this.el.hidden = false;
  }

  /** The left and right edges (window x) of the scrollport a sideways-scrolling character is clipped by, else null:
   *  the scroller's padding box, inside its border and left of a vertical scroll bar, which is where its content is
   *  cut. */
  private clipBox(parent: Element): { left: number; right: number } | null {
    const scroller = parent.closest(SCROLLER);
    if (!scroller || !this.root.contains(scroller)) return null;
    const left = scroller.getBoundingClientRect().left + scroller.clientLeft;
    return { left, right: left + scroller.clientWidth };
  }

  private valid(c: Caret | null): c is Caret {
    return c !== null && c.node.isConnected && this.root.contains(c.node) && c.offset < c.node.data.length;
  }

  private inBand(c: Caret, band: { top: number; bottom: number }): boolean {
    const r = this.rectOf(c);
    if (!r) return false;
    const mid = (r.top + r.bottom) / 2 + window.scrollY;
    return mid >= band.top && mid <= band.bottom;
  }

  /** The character's box (window coordinates), or null when it draws nothing: white space the engine collapsed, a
   *  line break in code. */
  private rectOf(c: Caret): DOMRect | null {
    const r = document.createRange();
    r.setStart(c.node, c.offset);
    r.setEnd(c.node, c.offset + charLength(c.node, c.offset));
    for (const box of Array.from(r.getClientRects())) if (box.width > 0 && box.height > 0) return box;
    return null;
  }

  private shown(t: Text): boolean { return !t.parentElement?.closest(NOT_ON); }

  private walker(scope: Element = this.root): TreeWalker {
    return document.createTreeWalker(scope, NodeFilter.SHOW_TEXT, {
      acceptNode: (n) => ((n as Text).data !== "" && this.shown(n as Text) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP),
    });
  }

  /** The drawn character at a boundary point (`node`, `offset`), or the first one after it. */
  private caretFrom(node: Node, offset: number): Caret | null {
    const c = this.textCaret(node, offset);
    return c ? this.drawnFrom(c) : null;
  }

  /** The character at a boundary point (`node`, `offset`), or the first one after it, drawn or not. */
  private textCaret(node: Node, offset: number): Caret | null {
    if (!this.root.contains(node)) return null;
    if (node instanceof Text && offset < node.data.length && this.shown(node)) {
      return { node, offset: charStart(node, offset) };
    }
    // Past the end of a text node, or between an element's children: the first text after the point.
    const w = this.walker();
    let from: Node = node;
    if (!(node instanceof Text)) {
      const child = node.childNodes[offset];
      if (child) {
        if (child instanceof Text && child.data !== "" && this.shown(child)) return { node: child, offset: 0 };
        from = child;
      } else while (from.lastChild) from = from.lastChild;
    }
    w.currentNode = from;
    const text = w.nextNode() as Text | null;
    return text ? { node: text, offset: 0 } : null;
  }

  /** `c`, or the first character after it that is drawn. */
  private drawnFrom(c: Caret): Caret | null {
    return this.rectOf(c) ? c : this.stepDrawn(c, 1);
  }

  /** The next (1) or previous (-1) drawn character in the document's text order. */
  private stepDrawn(c: Caret, dir: 1 | -1): Caret | null {
    for (let n = this.stepSeen(c, dir); n; n = this.stepSeen(n, dir)) if (this.rectOf(n)) return n;
    return null;
  }

  /** The next (1) or previous (-1) character inside `within`, passing over text with no box (the folded content of a
   *  closed `details`) whole: the outermost element around it that has no box either is stepped past as one, not
   *  each of its text nodes and characters. */
  private stepSeen(c: Caret, dir: 1 | -1, within: Element = this.root): Caret | null {
    let n = this.step(c, dir, within);
    while (n && n.node !== c.node && !this.boxed(n.node)) n = this.stepPast(this.unboxed(n.node, within), dir, within);
    return n;
  }

  /** The outermost of `t` and the elements around it inside `within` that have no box. */
  private unboxed(t: Text, within: Element): Node {
    let out: Node = t;
    for (let e = t.parentElement; e && e !== within && within.contains(e); e = e.parentElement) {
      const r = e.getBoundingClientRect();
      if (r.width > 0 || r.height > 0) break;
      out = e;
    }
    return out;
  }

  /** The first (1) or last (-1) character of the text after or before `node` and all it holds, inside `within`. */
  private stepPast(node: Node, dir: 1 | -1, within: Element): Caret | null {
    const w = this.walker(within);
    let from = node;
    if (dir > 0) while (from.lastChild) from = from.lastChild;
    w.currentNode = from;
    const n = this.walk(w, dir);
    if (!n) return null;
    return { node: n, offset: dir > 0 ? 0 : charStart(n, n.data.length - 1) };
  }

  /** Whether the text node has a box at all, read at its ends (`text-from.ts::textSpan`). */
  private boxed(t: Text): boolean { return textSpan(t) !== null; }

  /** The next (1) or previous (-1) character in the text order of `within`, whether drawn or not. */
  private step(c: Caret, dir: 1 | -1, within: Element = this.root): Caret | null {
    if (dir > 0) {
      const next = c.offset + charLength(c.node, c.offset);
      if (next < c.node.data.length) return { node: c.node, offset: next };
    } else if (c.offset > 0) {
      return { node: c.node, offset: charStart(c.node, c.offset - 1) };
    }
    return this.stepPast(c.node, dir, within);
  }

  /* The block element a character belongs to: `h` and `l` never leave it (a list item's own text and its nested
     item are two), and a code block's line break ends a line. Text that no source-mapped block holds (raw HTML that
     the render marks with sibling boundaries, not around it) belongs to its nearest block-level element. */
  private scopeOf(c: Caret): Element {
    const parent = c.node.parentElement;
    return parent?.closest(".code-line") ?? parent?.closest(BLOCK_TAGS) ?? this.root;
  }

  /** The next (1) or previous (-1) drawn character in the same block element, not across a line break of code. The
   *  walk never leaves that element, so `h` at a paragraph's start reads nothing of what comes before it. */
  private stepLine(c: Caret, dir: 1 | -1): Caret | null {
    const scope = this.scopeOf(c);
    for (let n = this.stepSeen(c, dir, scope); n; n = this.stepSeen(n, dir, scope)) {
      if (this.scopeOf(n) !== scope) return null;
      if (n.node.data[n.offset] === "\n" && n.node.parentElement?.closest("pre")) return null;
      if (this.rectOf(n)) return n;
    }
    return null;
  }

  /** Where the kit's walker takes `motion` from `c`: a drawn character, or null for no move. */
  private byKit(c: Caret, motion: "w" | "b" | "e"): Caret | null {
    const k = kit();
    return this.landing(typeof k?.motion === "function" ? k.motion({ node: c.node, offset: c.offset }, motion) : null);
  }

  /** The drawn character a place the kit gives stands for: the one just after it; a place before a character that
   *  draws nothing, or one between elements, is a line's end, the line's last drawn character. Null for no place, one
   *  outside the page, and a line's end whose line has no drawn character (`lineEndsBetween`). */
  private landing(to: { node: Node; offset: number } | null): Caret | null {
    if (!to || !this.root.contains(to.node)) return null;
    const at = this.textCaret(to.node, to.offset);
    if (at && at.node === to.node && this.rectOf(at)) return at;
    const before = at ? this.stepDrawn(at, -1) : this.lastDrawn();
    return before && !this.lineEndsBetween(before, to) ? before : null;
  }

  /** Whether a line ends between the drawn character `c` and the place `to` after it, so `to` is on a later line
   *  that has no drawn character of its own: a `<br>`, a code block's line break, or an atom (`rows.ts::ATOM`: an
   *  image, a rule, a formula) drawn wholly below `c`'s line. What has no box (folded content), what the page does
   *  not show and the inside of an atom are passed over whole. */
  private lineEndsBetween(c: Caret, to: { node: Node; offset: number }): boolean {
    const line = this.rectOf(c);
    if (!line) return false;
    const range = document.createRange();
    range.setStart(c.node, c.offset + charLength(c.node, c.offset));
    range.setEnd(to.node, to.offset);
    const w = document.createTreeWalker(this.root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    w.currentNode = c.node;
    for (let n: Node | null = c.node; n && range.comparePoint(n, 0) <= 0;) {
      if (n instanceof Text) {
        const from = n === c.node ? range.startOffset : 0;
        const end = n === to.node ? to.offset : n.data.length;
        if (this.inCode(n) && n.data.slice(from, end).includes("\n")) return true;
      } else if (n instanceof Element) {
        if (n.tagName === "BR") return true;
        const r = n.matches(SKIP) ? null : n.getBoundingClientRect();
        const boxed = r !== null && (r.width > 0 || r.height > 0);
        const atom = boxed && n.matches(ATOM);
        if (atom && r.top >= line.bottom - 0.5) return true;
        if (!boxed || atom) {
          // Past all it holds: its last descendant, from which the walk goes on.
          let last: Node = n;
          while (last.lastChild) last = last.lastChild;
          w.currentNode = last;
        }
      }
      n = w.nextNode();
    }
    return false;
  }

  /** The last drawn character of the document, for a place after all its text. */
  private lastDrawn(): Caret | null {
    const w = this.walker();
    const t = w.lastChild() as Text | null;
    return t ? this.stepDrawn({ node: t, offset: t.data.length }, -1) : null;
  }

  /* The drawn character of `band`'s row holding window x `x`: the row's first text node whose last character on the
     row ends past `x`, found by halving over the row's text nodes, then the character by halving over that node's;
     else the row's last character (so does an infinite `x`, the end of the row `$` keeps). A row that runs right to
     left (a node's last character is at its left) is searched character by character for the nearest. */
  private charAtX(first: Text, band: Span, x: number): Caret | null {
    const row = this.row(first, band);
    const segs = row.segs;
    if (Number.isFinite(x) && this.rtl(row)) return this.nearest(segs, x);
    const rightOf = (s: Seg) => this.rectOf({ node: s.t, offset: s.tail })?.right ?? -Infinity;
    let lo = 0;
    for (let hi = segs.length; lo < hi;) {
      const m = (lo + hi) >> 1;
      if (rightOf(segs[m]) > x) hi = m;
      else lo = m + 1;
    }
    const s = segs[lo];
    if (!s) {
      const last = segs[segs.length - 1];
      return last ? { node: last.t, offset: last.tail } : null;
    }
    const end = s.tail + charLength(s.t, s.tail);
    const at = this.probe(s.t, this.lowerBound(s.t, s.head, end, (r) => r.right > x));
    return { node: s.t, offset: at && at.offset < end ? at.offset : s.tail };
  }

  /** Whether the row runs right to left: its own characters hold such a script (only the row's part of each node:
   *  one Hebrew comment in a code block that is one text node leaves its other lines as they are), or its text is
   *  set that way. Read once per row. */
  private rtl(row: Row): boolean {
    const parent = row.segs[0]?.t.parentElement;
    row.rtl ??= row.segs.some((s) => hasRtl(s.t.data.slice(s.head, s.tail + charLength(s.t, s.tail)))) ||
      (parent != null && getComputedStyle(parent).direction === "rtl");
    return row.rtl;
  }

  /** The drawn character of the row whose box is nearest window x `x`. */
  private nearest(segs: Seg[], x: number): Caret | null {
    let best: Caret | null = null;
    let gap = Infinity;
    for (const { t, head, tail } of segs) {
      for (let o = head; o <= tail; o += charLength(t, o)) {
        const r = this.rectOf({ node: t, offset: o });
        const d = !r ? Infinity : x < r.left ? r.left - x : x > r.right ? x - r.right : 0;
        if (d < gap) { gap = d; best = { node: t, offset: o }; }
      }
    }
    return best;
  }

  private mid(r: DOMRect): number { return (r.top + r.bottom) / 2 + window.scrollY; }

  /** The first drawn character at or after code unit `i` of `t`, with its box. A code block's line breaks draw
   *  nothing and are passed over unread, so a run of blank lines costs no reads. */
  private probe(t: Text, i: number): Drawn | null {
    const data = t.data;
    const code = this.inCode(t);
    for (let o = charStart(t, i); ; o += charLength(t, o)) {
      if (code) o = pastBreaks(data, o, 1);
      if (o >= data.length) return null;
      const r = this.rectOf({ node: t, offset: o });
      if (r) return { offset: o, r };
    }
  }

  /** The least code unit in [`lo`, `hi`) whose next drawn character satisfies `pred` (the box order of a row makes
   *  `pred` false and then true), else `hi`: found by halving. */
  private lowerBound(t: Text, lo: number, hi: number, pred: (r: DOMRect) => boolean): number {
    while (lo < hi) {
      const m = (lo + hi) >> 1;
      const p = this.probe(t, m);
      if (!p || p.offset >= hi || pred(p.r)) hi = m;
      else lo = p.offset + charLength(t, p.offset);
    }
    return lo;
  }

  /** The row of `band` that `t`, a text node with a character on it, is on: the text nodes of the block holding `t`
   *  that have characters on the band, in order (`text-from.ts::eachTextAt`, by the elements' own boxes), each with
   *  its first and its last drawn character there. The block is the table section for a cell's text (a taller cell's
   *  later line comes in document order before a shorter cell's first, a cell spanning rows sits beside the later
   *  rows' cells, and all are read), else the nearest source-mapped block, else the nearest block-level element. Read
   *  once per band and layout, while no text under the root changes: every key on the row uses the same reading. */
  private row(t: Text, band: Span): Row {
    if (this.changes.takeRecords().length > 0) this.rowRead = null;
    const scope = this.rowScope(t);
    const layout = this.layout();
    const k = this.rowRead;
    const same = k !== null && k.scope === scope &&
      (k.band === band || (layout !== null && k.layout === layout && k.band.top === band.top && k.band.bottom === band.bottom));
    if (same) return k;
    const segs: Seg[] = [];
    eachTextAt(scope, band, { skip: NOT_ON, blanks: true }, (n) => {
      const s = this.onBand(n, band);
      if (s) segs.push(s);
      return false;
    });
    const row: Row = { band, layout, scope, segs, at: new Map(segs.map((s, i) => [s.t, i])), before: [], drawn: new Map(), rtl: null };
    this.rowRead = row;
    return row;
  }

  private rowScope(t: Text): Element {
    const parent = t.parentElement;
    const scope = parent?.closest("tr")?.parentElement ?? parent?.closest(".code-line") ?? parent?.closest(BLOCK_TAGS);
    return scope && this.root.contains(scope) ? scope : this.root;
  }

  /** `t`'s part of the row of `band`, found by halving over its characters when it only partly lies there; null for a
   *  node with nothing drawn on the band. */
  private onBand(t: Text, band: Span): Seg | null {
    const head = this.probe(t, 0);
    const tail = head && this.lastDrawnIn(t, t.data.length - 1);
    if (!head || !tail || this.mid(tail.r) < band.top || this.mid(head.r) > band.bottom) return null;
    if (this.mid(head.r) >= band.top && this.mid(tail.r) <= band.bottom) return { t, head: head.offset, tail: tail.offset };
    const lo = this.lowerBound(t, 0, t.data.length, (r) => this.mid(r) >= band.top);
    const hi = this.lowerBound(t, lo, t.data.length, (r) => this.mid(r) > band.bottom);
    const from = this.probe(t, lo);
    const to = from && from.offset < hi ? this.lastDrawnIn(t, hi - 1) : null;
    return from && to ? { t, head: from.offset, tail: to.offset } : null;
  }

  private walk(w: TreeWalker, dir: 1 | -1): Text | null { return (dir > 0 ? w.nextNode() : w.previousNode()) as Text | null; }

  /** The last drawn character of `t` at or before code unit `i`; line breaks in code are passed over unread, as by
   *  `probe`. */
  private lastDrawnIn(t: Text, i: number): Drawn | null {
    const data = t.data;
    const code = this.inCode(t);
    for (let o = charStart(t, i); ; o = charStart(t, o - 1)) {
      if (code && data.charCodeAt(o) === 10) {
        o = pastBreaks(data, o, -1);
        if (o >= 0) o = charStart(t, o);
      }
      if (o < 0) return null;
      const r = this.rectOf({ node: t, offset: o });
      if (r) return { offset: o, r };
      if (o === 0) return null;
    }
  }
}
