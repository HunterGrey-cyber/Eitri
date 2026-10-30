import { reveal, type Bridge, type DocPrefixes, type EditorClosed, type EditorText, type EditorView, type PreviewSettings, type TextBody } from "../protocol";
import { createEngine, type Engine } from "../render/engine";
import type { MermaidRunner } from "../render/mermaid";
import { renderDocument } from "../render/pipeline";
import { kit } from "../kit";
import { trace } from "../trace";
import { ScrollGuard } from "./anchor";
import { clickedRecently, lineAt, pickBlock } from "./current";
import { buildIndex, lastLineAtOrBefore, lastTopAtOrBefore, measure, type Box } from "./lineindex";
import { blockedText, BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT, tooLargeText } from "./notices";
import {
  createThrottle, leader, lineToY, OwnTokens, REVEAL_THROTTLE_MS, scrollTarget, yToLine, type Leader,
} from "./sync";
import { applyUpdate } from "./update";

/** Why the listeners of `afterUpdate` are called: what depends on the rendered text (the kit's find) waits for
 *  `render`, what depends on the position or the keys (the HUD, the current block) follows every reason. */
export type UpdateReason = "render" | "view" | "scroll" | "anchor" | "focus" | "notice";

/* What is on the page: the buffer and tick it came from, the lines it was rendered from (what a yank copies), how
   many images it does not show, and the document route it was rendered with. */
interface Rendered { buf: number; tick: number; name: string; lines: string[]; blocked: number; route: DocPrefixes | null }

const docKey = (d: DocPrefixes | null) => (d ? `${d.root}\u0000${d.base}` : "");
const INSTANT = "instant" as ScrollBehavior;

/** The lines kept above the reading point when nvim does not say its 'scrolloff' (an older host): the agent panel's
 *  margin. */
export const DEFAULT_RESERVE_LINES = 2;
/** The reading reserve never takes more than this share of the view, however large 'scrolloff' is. */
const RESERVE_SHARE = 1 / 4;
/** The line height assumed where the root has none to read (no layout). */
const FALLBACK_LINE_PX = 22;
/** How long an edit jump owns nvim's cursor while the keys move to nvim: the host's focus event follows the jump. */
const EDIT_JUMP_MS = 1000;

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
  /** Images on the document route that failed to load, by URL, for the route they were seen under. */
  private failed = new Set<string>();
  private failedRoute = "";
  private readonly tokens = new OwnTokens();
  private readonly guard = new ScrollGuard();
  private readonly revealer = createThrottle(REVEAL_THROTTLE_MS, () => this.sendReveal());
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
    this.setNotice(EMPTY_TEXT);
    const scroll = () => this.onScroll();
    const wheel = () => this.onPointer();
    const resize = () => this.applyReserve();
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
      observer?.disconnect();
      this.revealer.cancel();
      this.cursorBar.remove();
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
    const reserveChanged = this.view?.scrolloff !== v.scrolloff;
    this.view = v;
    if (switched) {
      this.target = null; this.anchor = null; this.pointer = false; this.lastReveal = null; this.lastCursor = null;
      this.revealedTop = null; this.asked = null; this.placed = null; this.unsent = false; this.revealer.cancel();
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
    if (hasKeys && !hadKeys) this.pickCursorBlock();
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

  index(): Box[] { return (this.cached ??= buildIndex(measure(this.root, window.scrollY))); }
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
  fileName(): string { return this.view?.name ?? this.rendered?.name ?? ""; }
  isCurrent(): boolean { return this.view?.current ?? true; }
  blockedCount(): number { return this.rendered?.blocked ?? 0; }
  hasKeys(): boolean { return this.keys; }
  /** The reading reserve in px: nvim's 'scrolloff' in lines of the page's text (`DEFAULT_RESERVE_LINES` when nvim
   *  does not say), at most a quarter of the view. The reading point sits this far below the top, as nvim's cursor
   *  sits 'scrolloff' lines below its window's top, so the line being read is never on the top edge. */
  reservePx(): number {
    const lines = this.view?.scrolloff ?? DEFAULT_RESERVE_LINES;
    const line = parseFloat(getComputedStyle(this.root).lineHeight) || FALLBACK_LINE_PX;
    return Math.min(lines * line, window.innerHeight * RESERVE_SHARE);
  }
  /** Where the reader reads: the reading reserve below the top. Near the document's start the reserve shrinks to
   *  what is scrolled away, as 'scrolloff' keeps no margin above a buffer's first line, so the first blocks can be
   *  read (and yanked) at the top. */
  readingY(): number { return window.scrollY + Math.min(this.reservePx(), Math.max(0, window.scrollY)); }
  /** Where nvim put its cursor (zero-based) for the reading line at `y`, when the page's reveal of that reading line
   *  has been answered; else null. */
  heldLine(y: number): number | null {
    const reading = lineAt(this.index(), y);
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
  scrollByUser(dy: number, smooth: boolean): void { window.scrollBy({ top: dy, behavior: smooth ? "smooth" : INSTANT }); }
  /** Where the window is heading: the kit's running glide's destination, else where it is. */
  destination(): number { return kit()?.destination?.() ?? window.scrollY; }
  /** The window position that brings document y `top` to the reading reserve below the view's top. */
  yFor(top: number): number { return clamp(top - this.reservePx(), 0, this.scrollMax()); }
  /** Moves the window to `y` through the kit's glide (eased, or a jump under reduced motion or `smooth` off); the
   *  reader's scroll, so it reveals while leading. Without the kit's glide the engine's smooth scroll does it. */
  glideTo(y: number): void {
    const k = kit();
    if (typeof k?.scrollTo === "function") k.scrollTo(y);
    else window.scrollTo({ top: y, behavior: "smooth" });
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
    if (this.rendered && this.lead() === "preview" && (this.unsent || gliding)) this.sendReveal(true, false);
    this.unsent = false;
  }
  /** The edit jump moved nothing (no window shows the file, or the call failed): the keys stay, and the preview's
   *  scrolls reveal again. */
  editJumpFailed(): void { this.editJumpUntil = 0; }
  /** Brings an element to the reading reserve below the top; returns the window position it goes to. */
  scrollToElement(el: Element): number {
    const y = this.yFor(el.getBoundingClientRect().top + window.scrollY);
    this.glideTo(y);
    return y;
  }
  /** Where `G` takes the window: the document's last block at the bottom of the view, clear of the HUD, as vim's
   *  `G` puts the last line at the window's bottom; null with nothing rendered. */
  bottomY(): number | null {
    if (!this.rendered || this.root.hidden) return null;
    const count = this.rendered.lines.length;
    let end: number | null = null;
    for (const b of this.index()) if (b.line < count) end = Math.max(end ?? 0, b.top + b.height);
    if (end === null) return null;
    const hud = document.getElementById("eitri-keys-hud");
    const clear = hud && !hud.hidden ? hud.getBoundingClientRect().height : 0;
    return clamp(end + clear - window.innerHeight, 0, this.scrollMax());
  }
  afterUpdate(fn: (why: UpdateReason) => void): () => void { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; }

  private editJumping(): boolean { return Date.now() < this.editJumpUntil; }
  private lead(): Leader { return leader({ sync: this.settings.sync, hasKeys: this.keys, pointerScroll: this.pointer }); }
  private scrollMax(): number { return Math.max(0, document.documentElement.scrollHeight - window.innerHeight); }

  /* The engine's own scrolls to a target (a focused link, a fragment) keep the same reserve at the top, through
     `scroll-padding-top` (markdown.css). */
  private applyReserve(): void {
    document.documentElement.style.setProperty("--mdp-reserve", `${Math.round(this.reservePx())}px`);
  }
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
    // The reader's line, read before the update moves anything.
    const before = !switched && this.lead() !== "editor" ? (this.anchor ?? yToLine(this.index(), window.scrollY)) : null;
    const doc = this.view?.buf === t.buf ? this.view.doc : null;
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
     is shown as outside the project, and stays so on later renders of the same route rather than flashing a broken
     image each time. Returns how many it boxed. */
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
     `keepCursor`, the place is moved the least that keeps nvim's cursor block in view (`keepCursorIn`). Returns the
     window position used, or null with nothing to place. */
  private scrollToLine(L: number, keepCursor = false): number | null {
    const index = this.index();
    if (index.length === 0) return null;
    const max = this.scrollMax();
    let y = scrollTarget(lineToY(index, L, max));
    if (keepCursor) y = this.keepCursorIn(index, y, max);
    if (Math.abs(y - window.scrollY) >= 1) { this.guard.arm(y); window.scrollTo({ top: y, behavior: INSTANT }); }
    this.traceTop();
    return y;
  }

  /* While nvim leads, the preview shows nvim's top line; rendered Markdown is taller than its source, so the line
     being typed lower in nvim's window can sit below the preview's view. Like 'scrolloff', the least scroll then
     keeps nvim's cursor line, as placed in the block that holds it (a block's lines share its height), inside the
     view with the reading reserve; of a line taller than the view, its top part. Above the view it acts only once the
     line is wholly out of it: nvim's top is the preview's top, so a line partly scrolled away there is nvim's own
     top line. Only this direction: nothing is revealed back, so nothing loops. */
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
    if (this.guard.onScroll(window.scrollY)) return;
    this.anchor = yToLine(this.index(), window.scrollY);
    if (this.lead() === "preview" && !this.editJumping()) { this.unsent = true; this.revealer.request(); }
    else this.traceTop();
    this.placeBars();
    this.notify("scroll");
  }

  private onPointer(): void {
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
    const reading = this.noCursor || !this.keys || !withCursor ? null : lineAt(index, this.readingY());
    const cursor = reading === null ? undefined : reading + 1;
    this.traceTop();
    if (line === this.lastReveal && (cursor === undefined || cursor === this.lastCursor)) return;
    this.lastReveal = line;
    this.revealedTop = line;
    if (cursor !== undefined) this.lastCursor = cursor;
    const token = this.tokens.next();
    if (cursor !== undefined) this.asked = { token, cursor };
    trace("reveal", cursor === undefined ? { line, token } : { line, cursor, token });
    const buf = this.rendered.buf;
    const args = cursor === undefined ? { buf, line, token } : { buf, line, token, cursor };
    reveal(this.bridge, args).catch((error: unknown) => {
      // A host from before `cursor` refuses the argument and moves nothing: reveal the top alone from now on, and
      // send this place again without it unless nvim has shown another top since or an edit jump owns its cursor.
      if (cursor === undefined || (error as { code?: unknown } | null)?.code !== "bad_request") return;
      this.noCursor = true;
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
    const jumped = this.editJumping();
    this.editJumpUntil = 0;
    if (jumped) { this.unsent = false; return; }
    if (this.unsent || gliding) this.sendReveal(true);
  }

  /* The keys came into the preview: when nvim's cursor line is on screen and no click just picked a block, the
     block holding it becomes the current block until the next scroll, with nvim's line kept for Enter. A line no
     block holds (a blank line between paragraphs, a line of a hidden element) keeps its own line too, and the nearest
     block by line stands as the current one: the block above on a tie, as a blank line closes the paragraph above. */
  private pickCursorBlock(): void {
    const line = this.editorCursorLine();
    if (line === null || clickedRecently() || !this.rendered) return;
    const index = this.index();
    const count = this.rendered.lines.length;
    const i = lastLineAtOrBefore(index, line);
    if (i >= 0 && line < index[i].end) {
      const box = index[i];
      if (box.top >= window.scrollY + window.innerHeight || box.top + box.height <= window.scrollY) return;
      pickBlock(box, line);
      return;
    }
    const y = lineToY(index, line, Number.POSITIVE_INFINITY);
    if (y < window.scrollY || y >= window.scrollY + window.innerHeight) return;
    // The end sentinel stands for no source line, so it is never the nearest block.
    const above = i >= 0 && index[i].line < count ? index[i] : null;
    const below = i + 1 < index.length && index[i + 1].line < count ? index[i + 1] : null;
    const box = above && (!below || line - (above.end - 1) <= below.line - line) ? above : below;
    if (box) pickBlock(box, line);
  }

  /* nvim's cursor line, marked in the margin beside the block that holds it, only while nvim leads: with the keys in
     the preview, the reader's own place matters, not nvim's cursor. */
  private placeBars(): void {
    const v = this.view;
    const show = v !== null && this.rendered?.buf === v.buf && this.lead() === "editor";
    this.cursorBar.hidden = true;
    if (!show || !v) return;
    const index = this.index();
    const line = v.cursor.line - 1;
    const i = lastLineAtOrBefore(index, line);
    if (i < 0 || index[i].end <= line) return;
    const box = index[i];
    const left = this.root.getBoundingClientRect().left + window.scrollX - 12;
    Object.assign(this.cursorBar.style, { top: `${box.top}px`, height: `${box.height}px`, left: `${left}px` });
    this.cursorBar.hidden = false;
  }
}
