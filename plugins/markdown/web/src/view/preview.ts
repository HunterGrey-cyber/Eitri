import { reveal, type Bridge, type DocPrefixes, type EditorClosed, type EditorText, type EditorView, type PreviewSettings, type TextBody } from "../protocol";
import { createEngine, type Engine } from "../render/engine";
import type { MermaidRunner } from "../render/mermaid";
import { renderDocument } from "../render/pipeline";
import { kit } from "../kit";
import { trace } from "../trace";
import { ScrollGuard } from "./anchor";
import { BlockCursor, type BlockMotion } from "./block";
import { boxAt, clickedRecently, lineAt, pickBlock, pickedLine } from "./current";
import { RowCache, rowsIn, type Row } from "./rows";
import { textFrom } from "./text-from";
import { selectionBoxes, selectionSpan } from "./selection";
import { buildIndex, lastLineAtOrBefore, lastTopAtOrBefore, measure, type Box } from "./lineindex";
import { blockedText, BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT, tooLargeText } from "./notices";
import {
  createSettle, createThrottle, HELD_REVEAL_MAX_MS, HELD_REVEAL_QUIET_MS, leader, lineToY, OwnTokens, placeLine,
  REVEAL_THROTTLE_MS, rowAt, rowShare, scrollTarget, yToLine, type Leader,
} from "./sync";
import { applyUpdate } from "./update";

/** Why the listeners of `afterUpdate` are called: what depends on the rendered text (the kit's find) waits for
 *  `render`, what depends on the position or the keys (the HUD, the current block) follows every reason. */
export type UpdateReason = "render" | "view" | "scroll" | "anchor" | "focus" | "notice" | "cursor";

/* What is on the page: the buffer and tick it came from, the lines it was rendered from (what a yank copies), how
   many images it does not show, and the document route it was rendered with. */
interface Rendered { buf: number; tick: number; name: string; lines: string[]; blocked: number; route: DocPrefixes | null }

const docKey = (d: DocPrefixes | null) => (d ? `${d.root}\u0000${d.base}` : "");
const INSTANT = "instant" as ScrollBehavior;

/** The fewest lines of the page's text kept above the reading point, whatever nvim's 'scrolloff': Vim's own
 *  defaults.vim keeps 5, and nvim's 0 would leave the line being read on the view's top edge. */
export const RESERVE_FLOOR_LINES = 5;
/** The reading reserve never takes more than this share of the view, however large 'scrolloff' is. */
const RESERVE_SHARE = 1 / 4;
/** The line height assumed where the root has none to read (no layout). */
const FALLBACK_LINE_PX = 22;
/** How long an edit jump owns nvim's cursor while the keys move to nvim: the host's focus event follows the jump. */
const EDIT_JUMP_MS = 1000;
/** How long a smooth scroll asked of the engine is taken to be heading for its target. */
const ENGINE_SCROLL_MS = 1000;
/** How far below a source line's top the cursor looks for the row that line starts on: a line's top can fall on the
 *  boundary between two rows, and the row below it is the line's. */
const ROW_PROBE_PX = 0.5;

/** Where the cursor is: a row, or, in a row taller than the space between the margins, one line's height of it. */
export interface Band { top: number; bottom: number; row: Row }

function clamp(n: number, lo: number, hi: number): number { return Math.min(Math.max(n, lo), hi); }

/* The box an image that was not shown leaves, in the shape the sanitiser gives one, so a later render's box for the
   same image compares equal and is kept. Built with DOM calls: the alt text is the document's. */
function outsideBox(img: Element): Element {
  const d = img.ownerDocument;
  const outer = d.createElement("span");
  outer.className = "mdp-box";
  outer.setAttribute("data-eitri-blocked", "outside");
  const why = d.createElement("span");
  why.className = "mdp-box-reason";
  why.textContent = blockedText("outside");
  outer.append(why);
  const alt = img.getAttribute("alt") ?? "";
  if (alt !== "") outer.append(` ${alt}`);
  return outer;
}

/**
 * The preview of the followed buffer: renders each text nvim sends into the live document, and keeps the preview and
 * nvim showing the same lines.
 *
 * Only one side drives at a time (`leader`): while nvim leads, its top line is the target the preview scrolls to;
 * while the preview leads (it has the keys, or the reader scrolled it with the pointer), the reader's line is the
 * anchor and its changes go to nvim as throttled reveals. Whoever leads, an update or a height change keeps the line
 * that was at the top at the top, so typing above it or an image loading never moves what the reader is looking at.
 */
export class Preview {
  private readonly engine: Engine = createEngine();
  private readonly notice: HTMLElement;
  private readonly cursorBar: HTMLElement;
  /** nvim's newest view of the followed buffer. */
  private view: EditorView | null = null;
  private rendered: Rendered | null = null;
  /** The text the page shows, kept so a theme change or a late document route can render it again. */
  private lastText: TextBody | null = null;
  /** Only the newest text waits: a slow render never builds up a queue of stale ones. */
  private pending: EditorText | null = null;
  /** The waiting text is `lastText` again, not new text from nvim. */
  private pendingIsRerender = false;
  private scheduled = false;
  /** The line index, measured on first use after any change to the layout. */
  private cached: Box[] | null = null;
  private keys = false;
  private pointer = false;
  /** The line nvim shows at its top (zero-based, fractional), where the preview goes while nvim leads. */
  private target: number | null = null;
  /** The line at the top as last placed, by the reader's scroll or by applying nvim's target: what an update or a
   *  height change keeps at the top while nvim does not lead, and nvim's target when it takes the lead back. */
  private anchor: number | null = null;
  /** A buffer whose first view arrived before its text: its target is applied on its first render, whoever leads. */
  private applyOnRenderBuf: number | null = null;
  /** The top line nvim was last known to show, by its own view or by the page's last reveal: a reveal of that line
   *  would not move it. */
  private lastReveal: number | null = null;
  /** nvim's cursor line (one-based) as last known, by its own view or by the page's last reveal: a reveal moving the
   *  cursor there would not move it. */
  private lastCursor: number | null = null;
  /** The top line of the page's last reveal, until nvim reports another top of its own: while nvim's top is still
   *  that line, nvim has not moved, so taking the lead back keeps the preview's own (finer) position. */
  private revealedTop: number | null = null;
  /** The host refused a reveal's `cursor` (an older host): reveals carry only the top line from then on. */
  private noCursor = false;
  /** The host refused a reveal's `row` (an older host): reveals place nvim by its top line from then on. */
  private noRow = false;
  /** The screen row the page's last reveal asked nvim to start the reading line on. */
  private lastRow: number | undefined = undefined;
  /** The preview moved under its own lead since its last reveal: a reveal of where it is may still be owed. */
  private unsent = false;
  /** Until this time an edit jump (Enter, a double-click) owns nvim's cursor while the keys move to nvim: no scroll
   *  and no hand-off reveals over it. */
  private editJumpUntil = 0;
  /** The newest reveal that carried a cursor, and where nvim put that cursor (its echo's cursor line): nvim holds
   *  the reading line itself, or the nearest line it can without scrolling. */
  private asked: { token: number; cursor: number } | null = null;
  private placed: { asked: number; got: number } | null = null;
  private lastTop: number | null = null;
  /** The cursor, while the preview has the keys: the source line (zero-based, fractional) at the middle of its band,
   *  so it keeps its place in the text when the layout changes; null until it is placed. */
  private cursorAt: number | null = null;
  /** Counts the cursor's moves: what was picked for one place of the cursor holds until it moves. */
  private moves = 0;
  /** The cursor's band as last worked out, for the line index (the layout) it was worked out on. */
  /** How far the cursor's band's middle lies below `lineToY(cursorAt)`: several rows can read as one source line
   *  (footnotes below the index's last block all read as its end), and this tells them apart. */
  private cursorDy = 0;
  private bandMemo: { at: number; dy: number; index: Box[]; band: Band | null } | null = null;
  /** The leaf blocks the line index (as last built) leaves out: drawn below a later line's block. */
  private leftOut: Box[] = [];
  /** The rows of each index block, for the line index (the layout) they were read on. */
  private rowCache: RowCache | null = null;
  /** The top of the document's first row, for the line index it was read on. */
  private firstRowMemo: { index: Box[]; top: number } | null = null;
  /** Where a scroll asked of the engine (no kit glide) is heading, until it gets there, the reader scrolls, or a
   *  second has passed. */
  private engineTo: { y: number; until: number } | null = null;
  /** Ends `engineTo` when its second is up, so a scroll that stopped short is judged where it stopped. */
  private engineTimer: number | undefined = undefined;
  private lastScrollY = 0;
  /** The bottom of the document's last row, for the line index (the layout) it was read on. */
  private lastRowMemo: { index: Box[]; bottom: number | null } | null = null;
  private readonly cursorLine: HTMLElement;
  /** The block on one character of the cursor's band, drawn above it while the preview has the keys. */
  readonly block: BlockCursor;
  /** Visual line mode's band over the rows the selection spans. */
  private readonly visualBand: HTMLElement;
  /** Charwise visual mode's boxes over the selected characters. */
  private readonly visualBoxes: HTMLElement;
  /** What the visual band or boxes were last drawn for: the mode, the selection's ends and the layout. */
  private visualMemo: { kind: string; ends: [Node, number, Node, number]; index: Box[]; width: number } | null = null;
  /** Images that failed to load for the shown text and document route, by URL. */
  private failed = new Set<string>();
  private failedRoute = "";
  private readonly tokens = new OwnTokens();
  private readonly guard = new ScrollGuard();
  private readonly revealer = createThrottle(REVEAL_THROTTLE_MS, () => this.sendReveal());
  /** A held key is repeating the cursor's moves: until its repeats stop, the moves and the scrolls they make reach
   *  nvim through `heldRevealer`, not one reveal per repeat. */
  private held = false;
  private readonly heldRevealer = createSettle(HELD_REVEAL_QUIET_MS, HELD_REVEAL_MAX_MS, (stopped) => {
    if (stopped) this.held = false;
    this.sendReveal();
  });
  private readonly listeners = new Set<(why: UpdateReason) => void>();
  private readonly disposers: (() => void)[] = [];

  constructor(private readonly root: HTMLElement, readonly bridge: Bridge, private readonly settings: PreviewSettings,
    private readonly mermaid: MermaidRunner) {
    let notice = document.getElementById("mdp-notice");
    if (!notice) {
      notice = document.createElement("div");
      notice.id = "mdp-notice";
      notice.className = "mdp-notice";
      root.before(notice);
    }
    this.notice = notice;
    this.cursorBar = document.createElement("div");
    this.cursorBar.className = "mdp-cursor-bar";
    this.cursorBar.hidden = true;
    document.body.append(this.cursorBar);
    // The row band at the cursor, as nvim's 'cursorline' marks the cursor's line; drawn under the bar.
    this.cursorLine = document.createElement("div");
    this.cursorLine.className = "mdp-cursor-line";
    this.cursorLine.hidden = true;
    this.cursorBar.before(this.cursorLine);
    this.visualBand = document.createElement("div");
    this.visualBand.className = "mdp-visual-band";
    this.visualBand.hidden = true;
    this.cursorLine.before(this.visualBand);
    this.visualBoxes = document.createElement("div");
    this.visualBoxes.className = "mdp-visual-boxes";
    this.visualBand.before(this.visualBoxes);
    // After the band and the bar, so it is painted above the band, whose blending would wash out its glyph.
    this.block = new BlockCursor(root, () => this.cached);
    this.setNotice(EMPTY_TEXT);
    const scroll = () => this.onScroll();
    const wheel = () => this.onPointer();
    const resize = () => this.applyReserve();
    // A block scrolled sideways (a wide table or code block) moves the text under the block cursor and visual mode's
    // boxes; its scroll does not reach the window, so it is caught on the way down.
    const nestedScroll = (ev: Event) => {
      if (ev.target === this.root) return;
      this.drawBlock();
      if (!document.documentElement.hasAttribute("data-eitri-visual")) return;
      this.visualMemo = null;
      this.placeVisual();
    };
    root.addEventListener("scroll", nestedScroll, true);
    // A press right of the content is on the scroll bar: dragging it is a pointer scroll like the wheel.
    const down = (ev: PointerEvent) => { if (ev.clientX >= document.documentElement.clientWidth) this.onPointer(); };
    // Image events do not bubble; the capture phase sees every image under the root.
    const loaded = () => this.reanchor();
    const failed = (ev: Event) => { this.imageFailed(ev.target); this.reanchor(); };
    window.addEventListener("scroll", scroll, { passive: true });
    window.addEventListener("wheel", wheel, { passive: true });
    window.addEventListener("resize", resize);
    window.addEventListener("pointerdown", down);
    root.addEventListener("load", loaded, true);
    root.addEventListener("error", failed, true);
    // Any height change under the root (a font arriving, the window resized) moves the blocks under the reader.
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(loaded) : null;
    observer?.observe(root);
    this.applyReserve();
    this.disposers.push(() => {
      window.removeEventListener("scroll", scroll);
      window.removeEventListener("wheel", wheel);
      window.removeEventListener("resize", resize);
      window.removeEventListener("pointerdown", down);
      root.removeEventListener("load", loaded, true);
      root.removeEventListener("error", failed, true);
      root.removeEventListener("scroll", nestedScroll, true);
      observer?.disconnect();
      this.revealer.cancel();
      this.endHeld();
      this.endEngineScroll();
      this.cursorBar.remove();
      this.cursorLine.remove();
      this.visualBand.remove();
      this.visualBoxes.remove();
      this.block.remove();
    });
  }

  /** Removes the page listeners and the cursor bar. */
  dispose(): void { for (const fn of this.disposers.splice(0)) fn(); this.listeners.clear(); }

  /**
   * A view from nvim. A view of another buffer is a switch: what was known of the old buffer's position goes, and the
   * new buffer's first view is applied whoever leads, as soon as its text is on the page. A view answering one of
   * the page's own reveals is never applied (it would pull the preview back to where it was). Any other view moves
   * the preview while nvim leads, unless nvim's top is still the line the page last revealed: nvim has not scrolled
   * since, and the preview stays where the reader left it, inside a wrapped block included. nvim's own view also
   * ends a pointer scroll's lead.
   */
  onView(v: EditorView): void {
    const switched = this.view?.buf !== v.buf;
    const docChanged = this.rendered !== null && this.rendered.buf === v.buf && docKey(this.rendered.route) !== docKey(v.doc);
    const reserveChanged = this.view?.scrolloff !== v.scrolloff || this.view?.rows !== v.rows;
    this.view = v;
    if (switched) {
      this.target = null; this.anchor = null; this.pointer = false; this.lastReveal = null; this.lastCursor = null;
      this.revealedTop = null; this.asked = null; this.placed = null; this.unsent = false; this.revealer.cancel();
      this.endHeld();
      this.lastRow = undefined;
    }
    const echo = v.cause === "reveal" && v.token !== undefined && this.tokens.has(v.token);
    if (echo && this.asked !== null && v.token === this.asked.token) {
      this.placed = { asked: this.asked.cursor, got: v.cursor.line };
    }
    if (!echo && v.cause === "user") this.pointer = false;
    // nvim moved on its own, so a reveal of a line the page revealed before may be needed again.
    if (!echo) {
      this.lastReveal = v.top;
      this.lastCursor = v.cursor.line;
      this.lastRow = undefined;
      if (v.top !== this.revealedTop) this.revealedTop = null;
    }
    if (switched || (!echo && this.lead() === "editor")) {
      const kept = !switched && this.revealedTop === v.top && v.top_fraction === 0 && this.anchor !== null &&
        Math.floor(this.anchor) + 1 === v.top;
      this.target = kept ? this.anchor : v.top - 1 + v.top_fraction;
      if (this.rendered?.buf === v.buf) this.applyTarget();
      else this.applyOnRenderBuf = v.buf;
    }
    if (reserveChanged) this.applyReserve();
    // Images resolved before the buffer had a document route (or under an old one) resolve again under the new one.
    if (docChanged) this.rerender();
    this.placeBars();
    this.notify("view");
  }

  /** A text from nvim, rendered on the next turn of the event loop. */
  onText(t: EditorText): void {
    this.pending = t;
    this.pendingIsRerender = false;
    this.schedule();
  }

  /** The buffer or the editor went away. The last render stays, under a notice saying so. */
  onClosed(c: EditorClosed): void {
    if (c.reason === "editor") this.setNotice(EDITOR_CLOSED_TEXT);
    else if (c.buf === this.view?.buf) this.setNotice(BUFFER_CLOSED_TEXT);
    this.notify("notice");
  }

  /**
   * Where the keys are. Nothing scrolls when they move: the preview stays where the reader left it. When the preview
   * takes the lead, the line shown now becomes its anchor, so a height change before the reader scrolls keeps that
   * line rather than one from an earlier lead. When nvim takes the lead back, the line shown now becomes its target
   * until nvim's next view, so a height change in between does not jump to a line nvim showed before the preview had
   * the keys.
   *
   * When the preview loses the lead, a glide still running ends at once where it was heading, and if the preview
   * moved since its last reveal nvim is told the final place, so nvim's top and cursor are where the reader stopped;
   * a preview that did not move sends nothing, and leaving through an edit jump sends nothing over the jump's cursor.
   * When the keys come into the preview and
   * nvim's cursor line is on screen, the block holding it (or the nearest block, for a line no block holds) becomes
   * the current block (as a click makes it), so `yy` acts on that block and Enter keeps nvim's own line.
   */
  onFocus(hasKeys: boolean): void {
    const was = this.lead();
    const hadKeys = this.keys;
    const next = leader({ sync: this.settings.sync, hasKeys, pointerScroll: this.pointer });
    if (was === "preview" && next !== "preview") this.handOff();
    if (hasKeys) this.editJumpUntil = 0;
    this.keys = hasKeys;
    this.leadChanged(was);
    // The cursor starts on nvim's cursor line, unless a click that brought the keys has just put it: nvim hears of
    // that place now, since the click moved the cursor while nvim still led.
    if (hasKeys && !hadKeys) {
      if (!clickedRecently()) { this.block.reset(); this.startCursor(); }
      else if (this.cursorAt !== null && !this.editJumping()) { this.unsent = true; this.revealer.request(); }
    }
    this.placeBars();
    this.notify("focus");
  }

  /** Renders the text on the page again, keeping any notice shown over it: after a theme change (diagrams carry their
   *  colours inside) or when the document route arrives after the text. */
  rerender(): void {
    if (!this.lastText || this.pending) return;
    this.pending = this.lastText;
    this.pendingIsRerender = true;
    this.schedule();
  }

  index(): Box[] {
    if (this.cached) return this.cached;
    const measured = measure(this.root, window.scrollY);
    const index = buildIndex(measured);
    const kept = new Set(index.map((b) => b.el));
    // The leaf blocks the index leaves out because they are drawn below a later line's block (footnotes): a row of
    // theirs reads as their own line, not as the gap's.
    this.leftOut = measured
      .filter((m) => m.visible && m.height > 0 && !m.hasTaggedVisibleDescendant && !kept.has(m.el))
      .map((m) => ({ line: m.line, end: m.end !== null && m.end > m.line ? m.end : m.line + 1, top: m.top,
        height: m.height, el: m.el }));
    this.cached = index;
    return index;
  }
  /**
   * The source line at the top (zero-based, fractional), what a page that replaces this one is given to show: the
   * line last placed there, while the page still shows it, else the line read at the scroll offset. Read back from
   * the offset each time, the line would lose the part of a pixel the browser drops from the offset, and a line
   * carried between two windows of different widths would lose it again at every move.
   */
  readingLine(): number {
    if (this.anchor !== null && Math.abs(this.offsetFor(this.anchor) - window.scrollY) < 1) return this.anchor;
    return yToLine(this.index(), window.scrollY);
  }
  /** Shows source line `L` at the top as the line placed there, as a page restoring its predecessor's line does:
   *  while nvim leads it is also the target until nvim's next view. */
  showLine(L: number): void {
    if (this.lead() === "editor") this.target = L;
    this.anchor = L;
    this.scrollToLine(L);
  }
  lines(): string[] { return this.rendered?.lines ?? []; }
  followedBuf(): number | null { return this.view?.buf ?? this.rendered?.buf ?? null; }
  /** The buffer and file the page's blocks were rendered from. A view of another buffer arrives before its text, so
   *  for a moment nvim follows one buffer while the page still shows another: what acts on the content (a link, a
   *  jump to a line, a history entry) names the shown one. */
  shownBuf(): number | null { return this.rendered?.buf ?? null; }
  shownName(): string { return this.rendered?.name ?? ""; }
  shownDoc(): DocPrefixes | null { return this.rendered?.route ?? null; }
  fileName(): string { return this.view?.name ?? this.rendered?.name ?? ""; }
  isCurrent(): boolean { return this.view?.current ?? true; }
  blockedCount(): number { return this.rendered?.blocked ?? 0; }
  hasKeys(): boolean { return this.keys; }
  /** The reading reserve in px: nvim's 'scrolloff' in nvim's rows (the view's height over nvim's screen rows, so that
   *  nvim can start the line being read on the matching row; the page's line height from a host that does not say),
   *  and at least `RESERVE_FLOOR_LINES` lines of the page's text, at most a quarter of the view. The reading point
   *  sits this far below the top, so the line being read is never on the top edge. */
  reservePx(): number {
    const text = this.lineHeight();
    const rows = this.view?.rows;
    const row = rows ? (window.innerHeight * (1 - (this.view?.grid_top ?? 0))) / rows : text;
    const lines = Math.max((this.view?.scrolloff ?? 0) * row, RESERVE_FLOOR_LINES * text);
    return Math.min(lines, window.innerHeight * RESERVE_SHARE);
  }
  /** Where the reader reads: the middle of the cursor's band while the preview has the keys; without a cursor (nvim
   *  has the keys, nothing is shown) the reading reserve below the top, shrunk near the document's start. */
  readingY(): number {
    const b = this.cursorBand();
    if (b) return (b.top + b.bottom) / 2;
    return window.scrollY + Math.min(this.reservePx(), Math.max(0, window.scrollY));
  }
  /** Where nvim put its cursor (zero-based) for the reading line at `y`, when the page's reveal of that reading line
   *  has been answered; else null. */
  heldLine(y: number): number | null {
    const reading = this.sourceLineAt(y);
    const placed = this.placed;
    if (reading === null || placed === null || placed.asked !== reading + 1 || this.editorCursorLine() === null) {
      return null;
    }
    return placed.got - 1;
  }
  /** nvim's cursor line (zero-based) when nvim's newest view is of the buffer shown, else null. */
  editorCursorLine(): number | null {
    const v = this.view;
    return v !== null && this.rendered?.buf === v.buf ? v.cursor.line - 1 : null;
  }
  /** nvim's cursor column as `editor.jump` takes it (one-based, in characters) when nvim's newest view is of the
   *  buffer shown, else null: the view says a byte offset, counted here over the line the page rendered. */
  editorCursorCol(): number | null {
    const v = this.view;
    if (v === null || this.rendered?.buf !== v.buf) return null;
    const text = this.rendered.lines[v.cursor.line - 1] ?? "";
    const before = new TextEncoder().encode(text).slice(0, v.cursor.col);
    return Array.from(new TextDecoder().decode(before)).length + 1;
  }
  /** A scroll the reader asked for. It is not guarded, so it is the reader's like any other and reveals while the
   *  preview leads. */
  scrollByUser(dy: number, smooth: boolean): void {
    // The reader's scroll replaces one the page asked of the engine: the window no longer heads where that one was.
    this.endEngineScroll();
    window.scrollBy({ top: dy, behavior: smooth ? "smooth" : INSTANT });
  }
  /** Where the window is heading: the kit's running glide's destination, else where it is. */
  destination(): number {
    const heading = kit()?.destination?.();
    if (typeof heading === "number") return heading;
    // Without the kit's glide the engine scrolls smoothly toward where the page asked it to go.
    const e = this.engineTo;
    if (e && Date.now() < e.until && Math.abs(window.scrollY - e.y) >= 1) return e.y;
    // There: over, its timer too. Its second up short of there: over, and the timer judges the cursor.
    if (e && Math.abs(window.scrollY - e.y) < 1) this.endEngineScroll();
    else this.engineTo = null;
    return window.scrollY;
  }
  /** The window position that brings document y `top` to the reading reserve below the view's top. */
  yFor(top: number): number { return clamp(top - this.reservePx(), 0, this.scrollMax()); }
  /** Brings an element to the reading reserve below the top, where a jump made without the cursor (nvim has the
   *  keys) lands; returns the window position it goes to. */
  scrollToElement(el: Element): number {
    const y = this.yFor(el.getBoundingClientRect().top + window.scrollY);
    this.glideTo(y);
    return y;
  }
  /** Moves the window to `y` through the kit's glide (eased, or a jump under reduced motion or `smooth` off); the
   *  reader's scroll, so it reveals while leading. Without the kit's glide the engine's smooth scroll does it. */
  glideTo(y: number): void {
    const k = kit();
    if (typeof k?.scrollTo === "function") k.scrollTo(y);
    else {
      const heading = { y, until: Date.now() + ENGINE_SCROLL_MS };
      this.endEngineScroll();
      this.engineTo = heading;
      // A scroll that has not got there in its second (the engine stopped it) is over: the cursor is judged where the
      // window is, and a margin there may push it.
      this.engineTimer = window.setTimeout(() => {
        this.engineTimer = undefined;
        this.engineTo = null;
        this.pushCursor();
      }, ENGINE_SCROLL_MS);
      window.scrollTo({ top: y, behavior: "smooth" });
    }
  }
  /** Forgets where a scroll asked of the engine was heading. */
  private endEngineScroll(): void {
    window.clearTimeout(this.engineTimer);
    this.engineTimer = undefined;
    this.engineTo = null;
  }
  /** Enter or a double-click is about to move nvim's cursor and give nvim the keys. A running glide ends where it was
   *  heading, so the block acted on is the one the reader is heading for; a place the preview has not yet revealed
   *  goes to nvim with its top line alone (the jump puts the cursor); and until the keys have gone (at most
   *  `EDIT_JUMP_MS`), neither a scroll nor the hand-off sends a reveal that would move nvim's cursor off the jump's
   *  line. */
  beforeEditJump(): void {
    this.editJumpUntil = Date.now() + EDIT_JUMP_MS;
    const gliding = Math.abs(this.destination() - window.scrollY) >= 1;
    kit()?.settle?.();
    this.revealer.cancel();
    this.endHeld();
    if (this.rendered && this.lead() === "preview" && (this.unsent || gliding)) this.sendReveal(true, false);
    this.unsent = false;
  }
  /** The edit jump moved nothing (no window shows the file, or the call failed): the keys stay, and the preview's
   *  scrolls reveal again. */
  editJumpFailed(): void { this.editJumpUntil = 0; }
  /** Where `G` takes the window: the document's last block at the bottom of the view, clear of the HUD, as vim's
   *  `G` puts the last line at the window's bottom; null with nothing rendered. */
  bottomY(): number | null {
    if (!this.rendered || this.root.hidden) return null;
    const count = this.rendered.lines.length;
    let end: number | null = null;
    for (const b of this.index()) if (b.line < count) end = Math.max(end ?? 0, b.top + b.height);
    if (end === null) return null;
    // Text the index leaves out (footnotes, shown below a later line's block) ends the document too.
    const index = this.index();
    if (this.lastRowMemo?.index !== index) this.lastRowMemo = { index, bottom: this.lastBand()?.row.bottom ?? null };
    if (this.lastRowMemo.bottom !== null) end = Math.max(end, this.lastRowMemo.bottom);
    return clamp(end + this.hudPx() - window.innerHeight, 0, this.scrollMax());
  }

  /** How many times the cursor has moved: what was picked for one place of the cursor holds until this changes. */
  cursorMoves(): number { return this.moves; }

  /** The cursor's band in document coordinates while the preview has the keys and shows a document, else null.
   *  Placed on first use: on nvim's cursor line, else on the first row below the top margin. */
  cursorBand(): Band | null {
    if (!this.keys || !this.rendered || this.root.hidden) return null;
    if (this.cursorAt === null) this.startCursor();
    if (this.cursorAt === null) return null;
    const index = this.index();
    const memo = this.bandMemo;
    if (memo && memo.at === this.cursorAt && memo.dy === this.cursorDy && memo.index === index) return memo.band;
    const band = this.bandAt(lineToY(index, this.cursorAt, Number.POSITIVE_INFINITY) + this.cursorDy);
    this.bandMemo = { at: this.cursorAt, dy: this.cursorDy, index, band };
    return band;
  }

  /** The source line (zero-based) at document y `y`. Inside a block, the line under it (`current.ts::lineAt`), moved
   *  up off the blank lines the block's map ends with (a list item followed by a blank line holds it), so the line
   *  nvim's cursor is put on, and Enter's, is one with the block's text. Between blocks (text no index block holds,
   *  such as a list item's own line above its nested list), the line read there, kept to the lines between the two
   *  blocks and off blank ones; only where there is none of those, the first line of the block below. */
  sourceLineAt(y: number): number | null {
    const index = this.index();
    const lines = this.rendered?.lines ?? [];
    const blank = (l: number) => (lines[l] ?? "").trim() === "";
    const box = boxAt(index, y);
    if (box && y >= box.top && y < box.top + box.height) {
      const line = lineAt(index, y);
      if (line === null) return null;
      let last = box.end - 1;
      while (last > box.line && blank(last)) last--;
      return Math.max(box.line, Math.min(line, last));
    }
    const i = lastTopAtOrBefore(index, y);
    const lo = i >= 0 ? index[i].end : 0;
    const hi = i + 1 < index.length ? index[i + 1].line - 1 : lines.length - 1;
    // A block drawn out of source order (a footnote, drawn after the last block whatever line defines it) is read as
    // its own lines: the gap's lines there may be text that draws nothing (a link's reference definition).
    const out = this.leftOut.find((b) => y >= b.top && y < b.top + b.height && (b.line < lo || b.line > hi));
    if (out) {
      const share = (y - out.top) / out.height;
      let last = out.end - 1;
      while (last > out.line && blank(last)) last--;
      return Math.min(out.line + Math.floor(share * (out.end - out.line)), last);
    }
    if (lo <= hi) {
      const read = Math.min(Math.max(Math.floor(yToLine(index, y)), lo), hi);
      for (let l = read; l <= hi; l++) if (!blank(l)) return l;
      for (let l = read - 1; l >= lo; l--) if (!blank(l)) return l;
    }
    return lineAt(index, y);
  }

  /** The cursor's place as a history entry keeps it (the source line at its band's middle), or null. */
  cursorPlace(): number | null { return this.cursorBand() ? this.cursorAt : null; }

  /** `j` / `k`: the cursor one row down (1) or up (-1), as nvim's `gj` / `gk` under 'wrap'. A move that would cross
   *  a margin scrolls the page by just enough instead; on the document's first or last row nothing moves. `held`: the
   *  key's repeat (see `moveCursor`). */
  moveRows(dir: 1 | -1, held = false): void {
    const b = this.cursorBand();
    const next = b ? this.stepBand(b, dir) : null;
    if (next) { this.carryColumn(next); this.moveCursor(next, this.scrollFor(next, this.destination()), held); }
  }

  /** `<C-d>` / `<C-u>`: the page half a view down or up and the cursor the same distance, so it keeps its height on
   *  the screen; where the page cannot go further (the document's start, or its last row at the bottom) the cursor
   *  moves alone. */
  halfPage(dir: 1 | -1, held = false): void {
    const b = this.cursorBand();
    if (!b) return;
    const s = this.destination();
    const d = window.innerHeight / 2;
    const t = dir > 0 ? Math.min(s + d, Math.max(s, this.endY())) : Math.max(s - d, 0);
    const moved = this.bandAt((b.top + b.bottom) / 2 + dir * d);
    const next = moved ? this.insideMargins(moved, t) : null;
    if (next) { this.carryColumn(next); this.moveCursor(next, this.scrollFor(next, t), held); }
  }

  /** `<C-f>` / `<C-b>`: the page a view less two lines down or up, and the cursor to the margin the text came in
   *  from (the top one going down, the bottom one going up), as nvim's; where the page cannot scroll, the cursor goes
   *  to the last or the first row. */
  fullPage(dir: 1 | -1, held = false): void {
    if (!this.cursorBand()) return;
    const s = this.destination();
    const line = this.lineHeight();
    const p = Math.max(line, window.innerHeight - 2 * line);
    const t = dir > 0 ? Math.min(s + p, Math.max(s, this.scrollMax())) : Math.max(s - p, 0);
    const next = Math.abs(t - s) < 0.5 ? (dir > 0 ? this.lastBand() : this.firstBand())
      : dir > 0 ? this.firstBandFrom(this.topLimit(t)) : this.lastBandTo(this.bottomLimit(t));
    if (next) { this.carryColumn(next); this.moveCursor(next, this.scrollFor(next, t), held); }
  }

  /** `gg` / `G`: the cursor to the document's first or last row, placed as a jump. False when there is none. */
  cursorFirst(): boolean { return this.jumpBand(this.cursorBand() ? this.firstBand() : null); }
  cursorLast(): boolean { return this.jumpBand(this.cursorBand() ? this.lastBand() : null); }

  /** Puts the cursor on the row at document y `y` and scrolls only if that row is outside the margins: as little
   *  as needed when it is near, centred when it is further than half a view away, as nvim places a jump. `from` is
   *  the window position the jump is judged from (where the window was heading before it). The block lands on the
   *  row's first character that is not blank, unless `keepBlock` says the caller has put it. False with no row. */
  jumpCursorTo(y: number, from = this.destination(), keepBlock = false): boolean {
    return this.cursorBand() !== null && this.jumpBand(this.bandAt(y), from, keepBlock);
  }

  /** The cursor to the row holding source line `L` (a history entry's place), placed as a jump. */
  jumpCursorToLine(L: number): boolean {
    const index = this.index();
    return this.cursorBand() !== null && this.jumpBand(this.bandOfLine(index, L, lineToY(index, L, Number.POSITIVE_INFINITY)));
  }

  /** `}` / `{`: the cursor to the first row of the next block below its row (`}`), or to the first row of the block
   *  it is in when it is below that row, else of the block above (`{`): the blocks the margin bar marks (the line
   *  index's), and a row between them (a list item's own text above its nested list) as a block of its own line. A
   *  jump, placed as `jumpCursorTo` places one. False when there is no such row. */
  blockJump(dir: 1 | -1): boolean {
    const b = this.cursorBand();
    if (!b) return false;
    const target = dir > 0 ? this.nextBlockStart(b) : this.blockStartBefore(b);
    return target !== null && Math.abs(target.top - b.top) >= 0.5 && this.jumpBand(target);
  }
  /** The index block holding band `b`'s middle, or null for a row between blocks. */
  private blockOfBand(b: Band): Box | null {
    const index = this.index();
    const mid = (b.top + b.bottom) / 2;
    const i = lastTopAtOrBefore(index, mid);
    if (i < 0 || mid >= index[i].top + index[i].height) return null;
    const box = index[i];
    // The end sentinel stands for no block.
    return i === index.length - 1 && !box.el.hasAttribute("data-line-end") ? null : box;
  }
  /** Whether bands `a` and `b` are in one block: one index block, or rows between blocks of one source line. */
  private sameBlock(a: Band, b: Band): boolean {
    const x = this.blockOfBand(a);
    const y = this.blockOfBand(b);
    if (x || y) return x === y;
    const line = (r: Band) => this.sourceLineAt((r.top + r.bottom) / 2);
    return line(a) !== null && line(a) === line(b);
  }
  /** The first row of the block after band `b`'s. */
  private nextBlockStart(b: Band): Band | null {
    const box = this.blockOfBand(b);
    let next = this.firstBandFrom(box ? box.top + box.height : b.bottom);
    for (let guard = 0; next && next.top > b.top + 0.5 && this.sameBlock(next, b) && guard < 1000; guard++) {
      next = this.stepBand(next, 1);
    }
    return next && next.top > b.top + 0.5 ? next : null;
  }
  /** The first row of band `b`'s block when `b` is below it, else of the block above. */
  private blockStartBefore(b: Band): Band | null {
    const start = (r: Band): Band => {
      const box = this.blockOfBand(r);
      let first = box ? this.firstBandFrom(box.top) ?? r : r;
      for (let guard = 0; guard < 1000; guard++) {
        const up = this.stepBand(first, -1);
        if (!up || up.top >= first.top - 0.5 || !this.sameBlock(up, r)) break;
        first = up;
      }
      return first;
    };
    const own = start(b);
    if (own.top < b.top - 0.5) return own;
    const up = this.stepBand(b, -1);
    return up && up.top < b.top - 0.5 ? start(up) : null;
  }

  /** Where visual mode starts (`v`, `V`): the block's character while it shows, else the first character of the
   *  cursor's row, or the first text after it when the row has none (an image); null without a cursor. */
  visualStart(): Range | null {
    if (this.blockOn()) {
      this.drawBlock();
      const at = this.block.caretRange();
      if (at) return at;
    }
    return this.rowStart();
  }
  /** The first character of the cursor's row that is not blank, or the first text after it when the row has none;
   *  null without a cursor. */
  private rowStart(b = this.cursorBand()): Range | null {
    if (!b) return null;
    const box = boxAt(this.index(), (b.top + b.bottom) / 2);
    const inBox = box !== null && b.top < box.top + box.height && b.bottom > box.top;
    return (inBox ? textFrom(box.el, b) : null) ?? textFrom(this.root, b);
  }
  /** Visual mode's cursor moved to the character at `rect` (window coordinates), `cursor` when the kit gives it: the
   *  block goes to the cursor's character (it stays there when the mode ends), the page's cursor goes to its row, and
   *  the page scrolls as for a jump, only when that row is outside the margins (a move along the row the kit's
   *  `<C-e>` scrolled away brings it back too). `cursor` is the kit's own, the same in `v` and `V` (where the
   *  selection's focus is at a line's end: a table row's is its last cell's); a line's end with no drawn character on
   *  its line leaves no block, and the cursor goes to `rect`'s row. A kit without it leaves the block to be read from
   *  the selection: charwise its focus, linewise the row of `rect`. */
  followSelection(rect: DOMRect, cursor?: { node: Node; offset: number }): void {
    const sel = document.getSelection();
    const linewise = document.documentElement.getAttribute("data-eitri-visual") === "line";
    const fromSelection = () => sel !== null && (linewise ? this.blockToLine(rect) : this.block.placeAtFocus(sel));
    const placed = this.blockOn() && (cursor ? this.block.placeAtCursor(cursor) : fromSelection());
    const at = (placed ? this.block.caretBox() : null) ?? rect;
    const b = this.cursorBand();
    const next = b ? this.bandAt((at.top + at.bottom) / 2 + window.scrollY) : null;
    if (b && next) {
      const from = this.destination();
      const outside = Math.abs(this.scrollFor(next, from) - from) >= 0.5;
      if (outside || Math.abs(next.top - b.top) >= 0.5) this.jumpBand(next, from, true);
    }
    if (placed) this.drawBlock();
    this.placeVisual();
  }

  /** The page draws visual mode's selection itself (the kit's highlight is hidden in `chrome.css`: WebKitGTK paints
   *  none on a list item's own text beside its nested list, which sits in an anonymous block). Linewise
   *  (`data-eitri-visual="line"` on the root): one band from the top of the selection's first row to the bottom of
   *  its last, across the text as the cursor's band is, a table row's whole box included: the lines are selected
   *  whole, the gaps between them too, as vim's `V` shows them. Charwise: a box over the selected characters of each
   *  row (`selectionBoxes`), a table cell's apart from the next cell's, each as tall as the row's band, so a row's
   *  boxes have one height and the rows of a paragraph meet. Drawn again only when the selection or the layout
   *  changed. */
  placeVisual(): void {
    const kind = document.documentElement.getAttribute("data-eitri-visual");
    const s = document.getSelection();
    const range = s && s.rangeCount > 0 ? s.getRangeAt(0) : null;
    if ((kind !== "line" && kind !== "char") || this.root.hidden || !range || !this.root.contains(range.startContainer) ||
      !this.root.contains(range.endContainer)) {
      this.visualBand.hidden = true;
      this.visualBoxes.replaceChildren();
      this.visualMemo = null;
      return;
    }
    const index = this.index();
    const rootBox = this.root.getBoundingClientRect();
    const ends: [Node, number, Node, number] = [range.startContainer, range.startOffset, range.endContainer, range.endOffset];
    const memo = this.visualMemo;
    if (memo && memo.kind === kind && memo.index === index && memo.width === rootBox.width &&
      memo.ends.every((v, i) => v === ends[i])) return;
    this.visualMemo = { kind, ends, index, width: rootBox.width };
    this.visualBand.hidden = true;
    this.visualBoxes.replaceChildren();
    const scheme = getComputedStyle(document.documentElement).getPropertyValue("--nv-color-scheme").trim() === "light"
      ? "light" : "dark";
    if (kind === "char") {
      const join = (parseFloat(getComputedStyle(this.root).fontSize) || 16) / 2;
      const boxes = selectionBoxes(range, join);
      // The rows the boxes lie on, read once (a long code block is one block: reading its rows per box would cost the
      // square of its length); the boxes come in row order, so one pass pairs them.
      let lo = Number.POSITIVE_INFINITY;
      let hi = Number.NEGATIVE_INFINITY;
      for (const b of boxes) { lo = Math.min(lo, b.top); hi = Math.max(hi, b.bottom); }
      const rows = boxes.length > 0
        ? rowsIn(this.root, index, lo, hi, this.lineHeight(), window.scrollY, this.rowsOf(index)) : [];
      let k = 0;
      for (const box of boxes) {
        const mid = (box.top + box.bottom) / 2;
        while (k + 1 < rows.length && rows[k].bottom <= mid) k++;
        const row = rows[k];
        const band = row && mid >= row.top && mid < row.bottom ? this.bandIn(row, mid) : null;
        const top = band ? Math.min(band.top, box.top) : box.top;
        const bottom = band ? Math.max(band.bottom, box.bottom) : box.bottom;
        const el = document.createElement("div");
        el.className = "mdp-visual-box";
        el.dataset.scheme = scheme;
        Object.assign(el.style, { top: `${top}px`, height: `${bottom - top}px`, left: `${box.left}px`,
          width: `${box.right - box.left}px` });
        this.visualBoxes.append(el);
      }
      return;
    }
    const span = selectionSpan(range);
    if (!span) return;
    const a = this.bandAt(span.first);
    const b = this.bandAt(span.last);
    if (!a || !b) return;
    let top = Math.min(a.top, b.top);
    let bottom = Math.max(a.bottom, b.bottom);
    for (const row of span.rows) {
      if (!row) continue;
      const box = row.getBoundingClientRect();
      top = Math.min(top, box.top + window.scrollY);
      bottom = Math.max(bottom, box.bottom + window.scrollY);
    }
    const left = rootBox.left + window.scrollX - 12;
    Object.assign(this.visualBand.style, { top: `${top}px`, height: `${bottom - top}px`, left: `${left}px`,
      width: `${rootBox.width + 12}px` });
    this.visualBand.dataset.scheme = scheme;
    this.visualBand.hidden = false;
  }

  /** A click at window x `x`, document y `y`: the cursor to the clicked row, without scrolling, and the block to the
   *  clicked character. It may come before the keys do; the cursor is then where they find it. */
  cursorToPoint(x: number, y: number): void {
    if (!this.rendered || this.root.hidden) return;
    const b = this.bandAt(y);
    if (!b) return;
    if (typeof kit()?.motion === "function" && this.block.placeNear(b, x, this.rowStart(b))) this.block.clearColumn();
    else this.block.reset();
    this.moveCursor(b, null);
  }

  /** Linewise visual mode's cursor is on the row of `rect`, from a kit that does not give its cursor: the kit draws
   *  whole lines, so the selection's focus is at that row's end (going forward) or start. The block stays where it is
   *  while it is on that row, else it goes to that row at its column, as `j` and `k` take it. False with no block to
   *  place. */
  private blockToLine(rect: DOMRect): boolean {
    const band = this.bandAt((rect.top + rect.bottom) / 2 + window.scrollY);
    const box = this.block.caretBox();
    if (!band || !box) return false;
    const mid = (box.top + box.bottom) / 2 + window.scrollY;
    if (mid >= band.top && mid <= band.bottom) return true;
    const x = this.block.column();
    return x !== null && this.block.placeNear(band, x, this.rowStart(band));
  }

  /** Whether the block cursor is in use: the preview has the keys and the kit has the motions that move it. */
  private blockOn(): boolean { return this.keys && typeof kit()?.motion === "function"; }

  /** The block's column onto the row of band `next`, before the cursor moves there (`j` `k` and the pages keep
   *  the x, as vim's curswant). A row with no character leaves the block hidden on it. */
  private carryColumn(next: Band): void {
    const x = this.blockOn() ? this.block.column() : null;
    if (x !== null) this.block.placeNear(next, x, this.rowStart(next));
  }

  /** One move of the block's character (`h` `l` `w` `b` `e` `0` `$`); true when it moved. Inside the row the band
   *  stays and nothing is sent to nvim; onto another row the cursor moves there as a jump does (the page scrolls only
   *  when the row is outside the margins, nvim hears of it as of any move). */
  blockMove(motion: BlockMotion): boolean {
    if (!this.blockOn() || !this.cursorBand()) return false;
    this.drawBlock();
    if (!this.block.move(motion)) return false;
    const box = this.block.caretBox();
    const b = this.cursorBand();
    const mid = box ? (box.top + box.bottom) / 2 + window.scrollY : null;
    if (b && mid !== null && (mid < b.top || mid > b.bottom)) this.jumpCursorTo(mid, undefined, true);
    this.drawBlock();
    return true;
  }

  /** A find moved to `match`: the block to its first character (the jump to its row follows, `jumpCursorTo` with
   *  `keepBlock`). */
  blockToMatch(match: Range): void { this.block.placeAt(match); }

  afterUpdate(fn: (why: UpdateReason) => void): () => void { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; }

  private editJumping(): boolean { return Date.now() < this.editJumpUntil; }

  private lineHeight(): number { return parseFloat(getComputedStyle(this.root).lineHeight) || FALLBACK_LINE_PX; }
  /** The height the keys' HUD takes at the view's bottom while it shows. */
  private hudPx(): number {
    const hud = document.getElementById("eitri-keys-hud");
    return hud && !hud.hidden ? hud.getBoundingClientRect().height : 0;
  }
  /** Where scrolling for the cursor stops going down: the document's last row at the view's bottom, as nvim's
   *  'scrolloff' never scrolls past a buffer's last line (`G`'s place). */
  private endY(): number { return this.bottomY() ?? this.scrollMax(); }
  /** The highest a cursor band may start with the window at `s`: the reserve below the view's top, giving way at
   *  the document's start (nvim keeps no margin above a buffer's first line). */
  private topLimit(s: number): number { return s <= this.firstRowTop() + 0.5 ? s : s + this.reservePx(); }
  /** The top of the document's first row (the page's top padding lies above it): with the window there or above,
   *  the document's start is in view and the top margin gives way, as 'scrolloff' does at a buffer's first line. */
  private firstRowTop(): number {
    const index = this.index();
    if (this.firstRowMemo?.index !== index) {
      this.firstRowMemo = { index, top: Math.max(0, this.firstBand()?.row.top ?? 0) };
    }
    return this.firstRowMemo.top;
  }
  /** The lowest a cursor band may end with the window at `s`: the reserve above the view's bottom (clear of the
   *  HUD), giving way once the window is at the document's end. */
  private bottomLimit(s: number): number {
    const bottom = s + window.innerHeight - this.hudPx();
    return s >= this.endY() - 0.5 ? bottom : bottom - this.reservePx();
  }
  /** The space between the two margins; a row taller than this is gone through a line's height at a time. */
  private room(): number {
    return Math.max(this.lineHeight(), window.innerHeight - this.hudPx() - 2 * this.reservePx());
  }
  private isTall(r: Row): boolean { return r.bottom - r.top > this.room() + 0.5; }

  /** The window position that keeps band `b` inside the margins, starting from `s` and moving the least. */
  private scrollFor(b: Band, s: number): number {
    if (b.top < this.topLimit(s) - 0.5) return Math.min(s, Math.max(0, b.top - this.reservePx()));
    if (b.bottom > this.bottomLimit(s) + 0.5) {
      const view = window.innerHeight - this.hudPx();
      return Math.max(s, Math.min(b.bottom - (view - this.reservePx()), Math.max(s, this.endY())));
    }
    return s;
  }
  /** As `scrollFor`, for a jump: a band further than half a view outside the margins is centred instead. */
  private scrollForJump(b: Band, s: number): number {
    const t = this.scrollFor(b, s);
    if (Math.abs(t - s) < 0.5) return s;
    const view = window.innerHeight - this.hudPx();
    const off = b.top < this.topLimit(s) ? this.topLimit(s) - b.top : b.bottom - this.bottomLimit(s);
    if (off <= view / 2) return t;
    const centre = (b.top + b.bottom) / 2 - view / 2;
    return centre < s ? Math.max(0, centre) : Math.min(centre, Math.max(s, this.endY()));
  }
  /** `b`, or, when it lies outside the margins with the window at `s`, the first or last band inside them: a move
   *  that keeps the window where it is pushes the cursor in, as 'scrolloff' does after nvim's `<C-d>`. */
  private insideMargins(b: Band, s: number): Band {
    const top = this.topLimit(s);
    const bottom = this.bottomLimit(s);
    if (b.top < top - 0.5) return this.firstBandFrom(top) ?? b;
    if (b.bottom > bottom + 0.5) return this.lastBandTo(bottom) ?? b;
    return b;
  }
  private jumpBand(b: Band | null, from = this.destination(), keepBlock = false): boolean {
    if (!b) return false;
    if (!keepBlock) this.block.reset();
    this.moveCursor(b, this.scrollForJump(b, from));
    return true;
  }

  /* The cursor goes to `b`; the window to `to` (null: it stays) through the kit's glide, as the reader's scroll;
     nvim hears of the move as it hears of a scroll. A `held` move (a key's repeat) goes there at once instead: a glide
     restarted by every repeat trails the cursor and keeps the page drawing a frame of it each refresh, after the key
     is let go too; and nvim hears of it when the repeats stop (`heldRevealer`). */
  private moveCursor(b: Band, to: number | null, held = false): void {
    // Before the window moves: its scroll is part of the same move.
    if (held) this.held = true;
    else this.endHeld();
    this.setCursor(b);
    const from = this.destination();
    if (to !== null && Math.abs(to - from) >= 0.5) {
      const y = this.onEditorRow(b, to, from);
      if (held) this.jumpWindow(y);
      else this.glideTo(y);
    }
    this.cursorMoved();
  }
  /** Moves the window to `y` at once, as the reader's scroll: a glide still running ends first, where it was heading. */
  private jumpWindow(y: number): void {
    kit()?.settle?.();
    this.endEngineScroll();
    if (Math.abs(y - window.scrollY) >= 0.5) window.scrollTo({ top: y, behavior: INSTANT });
  }
  /* A scroll the cursor makes, from `from` to `to`, carried on by less than one of nvim's rows so that the first row
     of the cursor's line lands where one of nvim's rows starts (`rowShare`): nvim can then start that line level with
     it, rather than as near as its whole rows allow. Going on in the scroll's own direction moves the cursor away
     from the margin it was crossing. Only while the view says nvim's rows, with the line's start on the screen, and
     never past the document's start or `G`'s place. */
  private onEditorRow(b: Band, to: number, from: number): number {
    const v = this.view;
    if (!v?.rows || v.buf !== this.rendered?.buf || !this.keys) return to;
    const line = this.sourceLineAt((b.top + b.bottom) / 2);
    if (line === null) return to;
    const view = window.innerHeight;
    const top = view * (v.grid_top ?? 0);
    const row = (view - top) / v.rows;
    const at = this.lineStartY(line, b) - to - top;
    if (row <= 0 || at < 0 || at > view - top) return to;
    // Further in the scroll's own direction first, then back; a position that would leave the band outside its
    // margins (a page move puts the cursor right at one) is not taken.
    const ks = to > from ? [Math.floor(at / row), Math.ceil(at / row)] : [Math.ceil(at / row), Math.floor(at / row)];
    for (const k of ks) {
      const snapped = to + at - k * row;
      if (snapped < 0 || snapped > Math.max(from, this.endY())) continue;
      if (b.top < this.topLimit(snapped) - 0.5 || b.bottom > this.bottomLimit(snapped) + 0.5) continue;
      return snapped;
    }
    return to;
  }
  private setCursor(b: Band): void {
    const index = this.index();
    const mid = (b.top + b.bottom) / 2;
    this.cursorAt = yToLine(index, mid);
    this.cursorDy = mid - lineToY(index, this.cursorAt, Number.POSITIVE_INFINITY);
    this.bandMemo = { at: this.cursorAt, dy: this.cursorDy, index, band: b };
    this.moves++;
  }
  private cursorMoved(): void {
    if (this.lead() === "preview" && !this.editJumping()) { this.unsent = true; this.requestReveal(); }
    this.placeBars();
    this.notify("cursor");
  }
  /** A reveal of where the preview is now: through the throttle, or, while a held key repeats, once it stops. */
  private requestReveal(): void {
    if (this.held) this.heldRevealer.request();
    else this.revealer.request();
  }
  /** The repeats are over (another key, the lead or the buffer changed): nothing waits for them to stop. */
  private endHeld(): void {
    this.held = false;
    this.heldRevealer.cancel();
  }

  /* The rows around document y `y`. Where none is within a view of it (below the document's end, or a gap taller
     than the view), the rows around the nearest block. */
  private rowsNear(y: number): Row[] {
    const index = this.index();
    const v = window.innerHeight;
    const line = this.lineHeight();
    const cache = this.rowsOf(index);
    const rows = rowsIn(this.root, index, y - v, y + v, line, window.scrollY, cache);
    if (rows.length > 0 || index.length === 0) return rows;
    const box = index[clamp(lastTopAtOrBefore(index, y), 0, index.length - 1)];
    return rowsIn(this.root, index, box.top - v, box.top + box.height + v, line, window.scrollY, cache);
  }
  /* The row cache for this layout: a block's rows are read once per layout, however often the cursor passes. */
  private rowsOf(index: Box[]): RowCache {
    if (this.rowCache?.index !== index) this.rowCache = new RowCache(index);
    return this.rowCache;
  }
  /** The band holding document y `y` (in a tall row, the line's height of it that holds `y`), else the nearest. */
  private bandAt(y: number): Band | null {
    const rows = this.rowsNear(y);
    let best: Row | null = null;
    let dist = Number.POSITIVE_INFINITY;
    for (const r of rows) {
      const d = y < r.top ? r.top - y : y >= r.bottom ? y - r.bottom + 1e-6 : 0;
      if (d < dist) { best = r; dist = d; }
      if (d === 0) break;
    }
    return best ? this.bandIn(best, y) : null;
  }
  private bandIn(r: Row, y: number): Band {
    if (!this.isTall(r)) return { top: r.top, bottom: r.bottom, row: r };
    const line = this.lineHeight();
    // A band's top is r.top plus a whole number of line heights; with a fractional line height the division can
    // come out a hair under that number, which must not round down to the band before.
    const k = Math.max(0, Math.floor((clamp(y, r.top, r.bottom - 0.01) - r.top) / line + 1e-6));
    const top = r.top + k * line;
    return { top, bottom: Math.min(top + line, r.bottom), row: r };
  }
  /** The band after (1) or before (-1) `b`: the next line's height of a tall row, else the next row. */
  private stepBand(b: Band, dir: 1 | -1): Band | null {
    const r = b.row;
    if (this.isTall(r)) {
      const top = b.top + dir * this.lineHeight();
      if (dir > 0 ? top < r.bottom - 0.5 : top > r.top - 0.5) return this.bandIn(r, Math.max(top, r.top));
    }
    const after = (rows: Row[]) => dir > 0 ? rows.find((x) => x.top >= r.bottom - 0.5)
      : [...rows].reverse().find((x) => x.bottom <= r.top + 0.5);
    let next = after(this.rowsNear(dir > 0 ? r.bottom : r.top));
    if (!next) {
      // Nothing with text within a view (a stretch taller than the view): the rows of the next block that way,
      // wherever in it they are.
      const index = this.index();
      const boxes = dir > 0 ? index.filter((x) => x.top >= r.bottom - 0.5)
        : index.filter((x) => x.top + x.height <= r.top + 0.5).reverse();
      for (const box of boxes) {
        next = after(rowsIn(this.root, index, box.top - 1, box.top + box.height + 1, this.lineHeight(), window.scrollY,
          this.rowsOf(index)));
        if (next) break;
      }
    }
    if (!next) return null;
    return this.bandIn(next, dir > 0 ? next.top : next.bottom);
  }
  private firstBand(): Band | null { return this.bandAt(0); }
  /** The document's last band: the end sentinel follows every block the page draws, footnotes included. */
  private lastBand(): Band | null {
    const index = this.index();
    if (index.length === 0) return null;
    const end = index[index.length - 1];
    return this.bandAt(end.top + end.height);
  }
  /** The band of source line `line` at document y `y` (where the line index puts it): the row nearest `y`, unless
   *  that row lies outside the line's own block while the block has a row from `y` down. A line's top can sit in its
   *  block's padding (a table row's top is the border above its cells' padding), nearer the text of the row above
   *  than its own. */
  private bandOfLine(index: Box[], line: number, y: number): Band | null {
    const band = this.bandAt(y);
    const i = lastLineAtOrBefore(index, line);
    if (!band || i < 0 || line >= index[i].end) return band;
    const box = index[i];
    const inBox = (b: Band) => b.bottom > box.top + 0.5 && b.top < box.top + box.height - 0.5;
    if (inBox(band)) return band;
    const below = this.firstBandFrom(Math.max(y, box.top));
    return below && inBox(below) ? below : band;
  }
  /** The first band starting at or below `y`, else the band nearest it. */
  private firstBandFrom(y: number): Band | null {
    const b = this.bandAt(y);
    return b && b.top < y - 0.5 ? this.stepBand(b, 1) ?? b : b;
  }
  /** The last band ending at or above `y`, else the band nearest it. */
  private lastBandTo(y: number): Band | null {
    const b = this.bandAt(y);
    return b && b.bottom > y + 0.5 ? this.stepBand(b, -1) ?? b : b;
  }
  /** The top of the first band of source line `line` that the cursor's band belongs to: where the line starts on
   *  the screen, which is where nvim starts it. Rows of one source line below the first share this place, so moving
   *  among them moves nothing in nvim, as `gj` inside one long line does not scroll. */
  private lineStartY(line: number, b: Band): number {
    const mid = (r: Row) => this.sourceLineAt((r.top + r.bottom) / 2);
    if (mid(b.row) !== line && this.sourceLineAt(b.row.top + ROW_PROBE_PX) !== line) return b.top;
    const rows = this.rowsNear(b.row.top);
    let i = rows.findIndex((r) => Math.abs(r.top - b.row.top) < 0.5);
    if (i < 0) return b.row.top;
    while (i > 0 && mid(rows[i - 1]) === line) i--;
    return rows[i].top;
  }

  /* The keys came into the preview: the cursor starts on the row of nvim's cursor line where the page shows it now,
     so nothing on the screen moves. Its block is picked with nvim's line, so Enter keeps that line until the cursor
     moves. A line no block holds (a blank line between paragraphs, a line of a hidden element) puts the cursor on
     the nearest block by line count, the block above on a tie (a blank line closes the paragraph above). With nvim's
     line off the screen (a host that does not say nvim's rows) or another buffer in nvim, the cursor starts on the
     first row below the top margin. */
  private startCursor(): void {
    if (!this.rendered || this.root.hidden) return;
    const index = this.index();
    const s = window.scrollY;
    const line = this.editorCursorLine();
    let band: Band | null = null;
    let picked: Box | null = null;
    if (line !== null && index.length > 0) {
      const v = this.view!;
      const count = this.rendered.lines.length;
      const i = lastLineAtOrBefore(index, line);
      if (i >= 0 && line < index[i].end) {
        const shown = line + (v.cursor.line === v.top ? v.top_fraction : 0);
        band = this.bandOfLine(index, line, lineToY(index, shown, Number.POSITIVE_INFINITY) + ROW_PROBE_PX);
        picked = index[i];
      } else {
        // A line between blocks that has a row of its own (a list item's own line above its nested list): that row.
        const y = lineToY(index, line, Number.POSITIVE_INFINITY) + ROW_PROBE_PX;
        const own = this.bandAt(y);
        if (own && y >= own.top && y < own.bottom && this.sourceLineAt((own.top + own.bottom) / 2) === line) {
          band = own;
          picked = boxAt(index, (own.top + own.bottom) / 2);
        }
      }
      if (!band && !(i >= 0 && line < index[i].end)) {
        // The end sentinel stands for no source line, so it is never the nearest block.
        const above = i >= 0 && index[i].line < count ? index[i] : null;
        const below = i + 1 < index.length && index[i + 1].line < count ? index[i + 1] : null;
        picked = above && (!below || line - (above.end - 1) <= below.line - line) ? above : below;
        if (picked) {
          band = picked === above ? this.bandAt(picked.top + picked.height - ROW_PROBE_PX)
            : this.bandAt(picked.top + ROW_PROBE_PX);
        }
      }
      if (band && (band.bottom <= s || band.top >= s + window.innerHeight)) { band = null; picked = null; }
    }
    band ??= this.firstBandFrom(this.topLimit(s));
    if (!band) return;
    this.setCursor(band);
    if (picked && line !== null) pickBlock(picked, line, this.moves);
  }

  /* A scroll the cursor did not ask for (the wheel, the scroll bar, <C-e>, a page's own scroll) leaves the cursor on
     its row unless a margin pushes it onto the first or last row inside, as nvim's mouse scroll and <C-e> do. It is
     judged where the window is heading, so a glide the cursor asked for never pushes the cursor back. */
  private pushCursor(): void {
    // In visual mode the cursor is the selection's, which only the kit moves (`followSelection`).
    if (!this.keys || this.cursorAt === null || document.documentElement.hasAttribute("data-eitri-visual")) return;
    const b = this.cursorBand();
    if (!b) return;
    const next = this.insideMargins(b, this.destination());
    if (Math.abs(next.top - b.top) < 0.5) return;
    this.setCursor(next);
    this.placeBars();
    this.notify("cursor");
  }
  private lead(): Leader { return leader({ sync: this.settings.sync, hasKeys: this.keys, pointerScroll: this.pointer }); }
  private scrollMax(): number { return Math.max(0, document.documentElement.scrollHeight - window.innerHeight); }

  /* The engine's own scrolls to a target (a focused link, a fragment) keep the same reserve at the top, through
     `scroll-padding-top` (markdown.css). */
  private applyReserve(): void {
    document.documentElement.style.setProperty("--mdp-reserve", `${Math.round(this.reservePx())}px`);
  }
  /** The page's text size moved (the host's `--nv-text-size` arrives in a theme event): the reserve is lines of that
   *  text, and nothing else about the page need have changed, so nothing else would refresh it. */
  onTextSize(): void { this.applyReserve(); }
  /** A theme event: the text size moved the reserve, and the block reads its blink times again. */
  onTheme(): void { this.onTextSize(); this.drawBlock(); }
  private notify(why: UpdateReason): void { for (const fn of [...this.listeners]) fn(why); }
  private setNotice(text: string): void { this.notice.textContent = text; if (text) trace("notice", { text }); }

  private schedule(): void {
    if (this.scheduled) return;
    this.scheduled = true;
    setTimeout(() => this.flush(), 0);
  }

  private flush(): void {
    this.scheduled = false;
    const t = this.pending;
    const again = this.pendingIsRerender;
    this.pending = null;
    this.pendingIsRerender = false;
    if (!t) return;
    if ("too_large" in t) {
      this.root.hidden = true;
      this.rendered = null;
      this.lastText = null;
      this.cached = null;
      this.setNotice(tooLargeText(t.too_large));
      // The cursor bar marks a block of the hidden document.
      this.placeBars();
      this.notify("notice");
      return;
    }
    const switched = this.rendered?.buf !== t.buf;
    // Another buffer's text: the cursor starts again in it (it stays on the old blocks until they go).
    if (switched) { this.cursorAt = null; this.block.reset(); }
    // The reader's line, read before the update moves anything.
    const before = !switched && this.lead() !== "editor" ? (this.anchor ?? yToLine(this.index(), window.scrollY)) : null;
    const doc = this.view?.buf === t.buf ? this.view.doc : null;
    if (this.lastText?.buf !== t.buf || this.lastText.text !== t.text) this.failed.clear();
    const result = renderDocument(this.engine, t, doc, this.settings, this.mermaid);
    const blocked = result.blocked + this.boxFailed(result.fragment, doc);
    this.root.hidden = false;
    applyUpdate(this.root, result.fragment);
    this.rendered = { buf: t.buf, tick: t.tick, name: t.name, lines: result.lines, blocked, route: doc };
    this.lastText = t;
    this.cached = null;
    if (!again) this.setNotice("");
    if (switched && window.scrollY >= 1) { this.guard.arm(0); window.scrollTo({ top: 0, behavior: INSTANT }); }
    const viewed = this.view?.buf === t.buf;
    if (viewed && this.target !== null && (this.applyOnRenderBuf === t.buf || this.lead() === "editor")) {
      this.applyOnRenderBuf = null;
      this.applyTarget();
    } else if (before !== null) {
      this.anchor = before;
      this.scrollToLine(before);
    }
    this.mermaid.renderPending(this.root, () => this.reanchor());
    this.placeBars();
    this.notify("render");
  }

  /* The document route answers a missing file and a file outside the project alike, so an image under it that fails
     is shown as outside the project, and stays so on re-renders of the same text and route rather than flashing a
     broken image each time. A text change lets it load again. Returns how many it boxed. */
  private boxFailed(fragment: DocumentFragment, doc: DocPrefixes | null): number {
    const key = docKey(doc);
    if (key !== this.failedRoute) { this.failed.clear(); this.failedRoute = key; }
    if (this.failed.size === 0) return 0;
    let n = 0;
    for (const img of fragment.querySelectorAll("img")) {
      const src = img.getAttribute("src");
      if (src !== null && this.failed.has(src)) { img.replaceWith(outsideBox(img)); n++; }
    }
    return n;
  }

  private imageFailed(target: EventTarget | null): void {
    const route = this.rendered?.route;
    if (!route || !(target instanceof Element) || target.tagName !== "IMG" || !this.root.contains(target)) return;
    const src = target.getAttribute("src");
    if (src === null || !src.startsWith(route.root)) return;
    this.failed.add(src);
    target.replaceWith(outsideBox(target));
    if (this.rendered) this.rendered.blocked++;
  }

  /* Heights changed under the reader: measure again and put the leading side's line back at the top. */
  private reanchor(): void {
    this.cached = null;
    if (this.lead() === "editor") { if (this.target !== null) this.scrollToLine(this.target, true); }
    else if (this.anchor !== null) this.scrollToLine(this.anchor);
    this.placeBars();
    this.notify("anchor");
  }

  /* Shows nvim's target at the top. It is also what is shown from now on, so it is the anchor until the reader
     scrolls: a later update or height change while the preview leads keeps it, not a line from before. */
  private applyTarget(): void {
    if (this.target === null) return;
    this.anchor = this.target;
    const y = this.scrollToLine(this.target, this.lead() === "editor");
    if (y !== null) this.anchor = yToLine(this.index(), y);
  }

  /* The lead moved: the side taking it starts from the line shown now. */
  private leadChanged(was: Leader): void {
    const now = this.lead();
    if (was === now || this.rendered === null) return;
    if (now === "editor") this.target = this.readingLine();
    else if (was === "editor") this.anchor = this.readingLine();
  }

  /* The page's own scrolls are instant (a smooth one would be read as the reader moving for its whole length) and
     guarded; a scroll to where the page already is fires no event, so it is not made and the guard not armed. With
     `forEditor` (nvim leads), the place is nvim's: its cursor line the same share down the view as that line's row is
     down nvim's screen (`cursorShare`), so `L` is not used; from a host that does not say nvim's rows, line `L` at the
     top moved the least that keeps nvim's cursor block in view (`keepCursorIn`). Returns the window position used, or
     null with nothing to place. */
  private scrollToLine(L: number, forEditor = false): number | null {
    const index = this.index();
    if (index.length === 0) return null;
    const max = this.scrollMax();
    const share = forEditor ? this.cursorShare() : null;
    let y: number;
    if (share !== null && this.view !== null) {
      // A cursor line that is nvim's top line, scrolled part way out of the window ('smoothscroll'), starts above
      // nvim's first row: the part still shown starts there, so the page puts that part at the share.
      const v = this.view;
      const shown = v.cursor.line - 1 + (v.cursor.line === v.top ? v.top_fraction : 0);
      y = scrollTarget(placeLine(index, shown, share, window.innerHeight, max));
    } else {
      y = scrollTarget(lineToY(index, L, max));
      if (forEditor) y = this.keepCursorIn(index, y, max);
    }
    if (Math.abs(y - window.scrollY) >= 1) { this.guard.arm(y); window.scrollTo({ top: y, behavior: INSTANT }); }
    this.traceTop();
    return y;
  }

  /* The share of nvim's screen above the row its cursor's line starts on, while the page shows nvim's buffer and the
     view says both numbers; else null (a host that does not say them). */
  private cursorShare(): number | null {
    const v = this.view;
    if (v === null || v.rows === undefined || v.cursor_row === undefined || v.rows < 1) return null;
    if (this.rendered?.buf !== v.buf) return null;
    return rowShare(v.cursor_row, v.rows, v.grid_top ?? 0);
  }

  /* From a host that does not say nvim's rows, while nvim leads the preview shows nvim's top line; rendered Markdown
     is taller than its source, so the line being typed lower in nvim's window can sit below the preview's view. Like
     'scrolloff', the least scroll then keeps nvim's cursor line, as placed in the block that holds it (a block's
     lines share its height), inside the view with the reading reserve; of a line taller than the view, its top part.
     Above the view it acts only once the line is wholly out of it: nvim's top is the preview's top, so a line partly
     scrolled away there is nvim's own top line. Only this direction: nothing is revealed back, so nothing loops. */
  private keepCursorIn(index: Box[], y: number, max: number): number {
    const line = this.editorCursorLine();
    if (line === null) return y;
    const i = lastLineAtOrBefore(index, line);
    if (i < 0 || index[i].end <= line) return y;
    const box = index[i];
    const reserve = this.reservePx();
    const view = window.innerHeight;
    const per = box.height / (box.end - box.line);
    const top = box.top + per * (line - box.line);
    const bottom = Math.min(top + per, top + Math.max(0, view - 2 * reserve));
    let out = y;
    if (bottom <= y) out = top - reserve;
    else if (bottom > y + view - reserve) out = bottom - view + reserve;
    return clamp(out, 0, max);
  }

  /* The scroll offset that shows line `L` at the top, within the range the page can scroll. */
  private offsetFor(L: number): number {
    const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    return scrollTarget(lineToY(this.index(), L, max));
  }

  private traceTop(): void {
    const index = this.index();
    const i = lastTopAtOrBefore(index, window.scrollY);
    const line = index[Math.max(0, i)]?.line ?? 0;
    if (line !== this.lastTop) { this.lastTop = line; trace("top", { line }); }
  }

  private onScroll(): void {
    // A smooth scroll asked of the engine is over once the window is there, or once something moved it away.
    const e = this.engineTo;
    if (e && (Math.abs(window.scrollY - e.y) < 1 || Math.abs(window.scrollY - e.y) > Math.abs(this.lastScrollY - e.y) + 1)) {
      // This scroll judges the cursor below (`pushCursor`), so the scroll's timer has nothing left to do.
      this.endEngineScroll();
    }
    this.lastScrollY = window.scrollY;
    if (this.guard.onScroll(window.scrollY)) return;
    this.anchor = yToLine(this.index(), window.scrollY);
    this.pushCursor();
    if (this.lead() === "preview" && !this.editJumping()) { this.unsent = true; this.requestReveal(); }
    else this.traceTop();
    this.placeBars();
    this.notify("scroll");
  }

  private onPointer(): void {
    // A wheel tick or the scroll bar takes over from a scroll the page asked of the engine, wherever it had got to.
    if (this.engineTo) {
      this.endEngineScroll();
      this.pushCursor();
    }
    if (this.settings.sync !== "both") return;
    const was = this.lead();
    this.pointer = true;
    this.leadChanged(was);
  }

  /* nvim moves by whole lines, so a reveal goes only when the line at the top or the reading line changed. The top
     is the exact line at the top. While the preview has the keys, the reading line goes as the reveal's `cursor`,
     which nvim keeps in view without scrolling, so moving the keys to nvim finds its cursor on the line being read;
     a pointer scroll while nvim has the keys moves nvim's window only, its cursor no further than 'scrolloff' needs.
     `force` sends the place whatever the lead (the lead has just gone); `withCursor` false sends the top alone. */
  private sendReveal(force = false, withCursor = true): void {
    if (!this.rendered || (!force && this.lead() !== "preview")) return;
    this.unsent = false;
    const index = this.index();
    const line = Math.max(1, Math.floor(yToLine(index, window.scrollY)) + 1);
    const band = this.noCursor || !this.keys || !withCursor ? null : this.cursorBand();
    const mid = band === null ? 0 : (band.top + band.bottom) / 2;
    // While the line nvim's cursor was on when the keys came in is still picked (the cursor has not moved), that line
    // is the one nvim keeps, so a scroll does not move nvim's cursor off it and Enter finds it where it was.
    const pickedBox = band === null ? null : boxAt(index, mid);
    const held = pickedBox === null ? null : pickedLine(pickedBox, this.moves);
    const reading = band === null ? null : held ?? this.sourceLineAt(mid);
    const cursor = reading === null ? undefined : reading + 1;
    // With the cursor's line, the screen row nvim starts it on: the same share down nvim's screen as the line's first
    // row is down the view, so the two sides show it at the same height.
    const rows = this.view?.buf === this.rendered.buf ? this.view.rows : undefined;
    const row = reading === null || band === null || this.noRow || !rows ? undefined
      : rowAt((held !== null ? lineToY(index, held, Number.POSITIVE_INFINITY) : this.lineStartY(reading, band))
        - window.scrollY, window.innerHeight, rows, this.view?.grid_top ?? 0);
    this.traceTop();
    const same = cursor === undefined || (cursor === this.lastCursor && row === this.lastRow);
    if (line === this.lastReveal && same) return;
    this.lastReveal = line;
    this.revealedTop = line;
    if (cursor !== undefined) { this.lastCursor = cursor; this.lastRow = row; }
    const token = this.tokens.next();
    if (cursor !== undefined) this.asked = { token, cursor };
    trace("reveal", cursor === undefined ? { line, token } : row === undefined ? { line, cursor, token }
      : { line, cursor, row, token });
    const buf = this.rendered.buf;
    const args = cursor === undefined ? { buf, line, token } : row === undefined ? { buf, line, token, cursor }
      : { buf, line, token, cursor, row };
    reveal(this.bridge, args).catch((error: unknown) => {
      // A host from before `row` (or `cursor`) refuses the argument and moves nothing: from now on reveals go without
      // it, and this place goes again so, unless nvim has shown another top since or an edit jump owns its cursor.
      if (cursor === undefined || (error as { code?: unknown } | null)?.code !== "bad_request") return;
      if (row !== undefined) this.noRow = true;
      else this.noCursor = true;
      if (this.lastReveal !== line || this.rendered?.buf !== buf || this.editJumping()) return;
      this.lastReveal = null;
      this.sendReveal(true);
    });
  }

  /* The preview is losing the lead: a glide still running ends where it was heading. When the preview moved since its
     last reveal (a trailing reveal still due, or the glide just ended), that place goes to nvim whatever the lead is
     now, so nvim does not keep a point the glide passed on the way; a preview that did not move sends nothing, so a
     focus round trip leaves nvim's cursor on the line being edited. Leaving through an edit jump sends nothing: the
     jump has put nvim's cursor. */
  private handOff(): void {
    const gliding = Math.abs(this.destination() - window.scrollY) >= 1;
    kit()?.settle?.();
    if (!this.rendered) return;
    this.anchor = yToLine(this.index(), window.scrollY);
    this.revealer.cancel();
    this.endHeld();
    const jumped = this.editJumping();
    this.editJumpUntil = 0;
    if (jumped) { this.unsent = false; return; }
    if (this.unsent || gliding) this.sendReveal(true);
  }

  /** Draws the block on its character of the cursor's band (after `block.placeAt`, a sideways scroll), while the
   *  preview has the keys and the kit has the motions that move it (kit 6); else hides it. */
  drawBlock(): void {
    const band = this.keys && typeof kit()?.motion === "function" ? this.cursorBand() : null;
    this.block.draw(band, () => this.rowStart());
  }
  /* With the keys in the preview, its cursor: the bar in the margin at the cursor's band, a faint band across the
     text, as nvim's 'cursorline', and the block on one character of it. While nvim leads, nvim's cursor line, marked
     in the margin beside the block that holds it. */
  private placeBars(): void {
    this.placeVisual();
    this.cursorBar.hidden = true;
    this.cursorLine.hidden = true;
    const rootBox = this.root.getBoundingClientRect();
    const left = rootBox.left + window.scrollX - 12;
    // A block drawn again keeps its blink running; one that goes away stops it.
    if (this.keys) {
      const b = this.cursorBand();
      if (!b) { this.block.hide(); return; }
      const height = `${b.bottom - b.top}px`;
      Object.assign(this.cursorBar.style, { top: `${b.top}px`, height, left: `${left}px` });
      Object.assign(this.cursorLine.style, { top: `${b.top}px`, height, left: `${left}px`,
        width: `${rootBox.width + 12}px` });
      const scheme = getComputedStyle(document.documentElement).getPropertyValue("--nv-color-scheme").trim();
      this.cursorLine.dataset.scheme = scheme === "light" ? "light" : "dark";
      this.cursorBar.hidden = false;
      this.cursorLine.hidden = false;
      this.drawBlock();
      return;
    }
    this.block.hide();
    const v = this.view;
    if (v === null || this.rendered?.buf !== v.buf || this.lead() !== "editor") return;
    const index = this.index();
    const line = v.cursor.line - 1;
    const i = lastLineAtOrBefore(index, line);
    if (i < 0 || index[i].end <= line) return;
    const box = index[i];
    Object.assign(this.cursorBar.style, { top: `${box.top}px`, height: `${box.height}px`, left: `${left}px` });
    this.cursorBar.hidden = false;
  }
}
