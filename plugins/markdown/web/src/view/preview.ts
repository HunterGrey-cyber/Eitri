import { reveal, type Bridge, type DocPrefixes, type EditorClosed, type EditorText, type EditorView, type PreviewSettings, type TextBody } from "../protocol";
import { createEngine, type Engine } from "../render/engine";
import type { MermaidRunner } from "../render/mermaid";
import { renderDocument } from "../render/pipeline";
import { trace } from "../trace";
import { ScrollGuard } from "./anchor";
import { buildIndex, lastLineAtOrBefore, lastTopAtOrBefore, measure, type Box } from "./lineindex";
import { blockedText, BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT, tooLargeText } from "./notices";
import { createThrottle, leader, lineToY, OwnTokens, REVEAL_THROTTLE_MS, yToLine, type Leader } from "./sync";
import { applyUpdate } from "./update";

/** Why the listeners of `afterUpdate` are called: what depends on the rendered text (the kit's find) waits for
 *  `render`, what depends on the position or the keys (the HUD, the current block) follows every reason. */
export type UpdateReason = "render" | "view" | "scroll" | "anchor" | "focus" | "notice";

/* What is on the page: the buffer and tick it came from, the lines it was rendered from (what a yank copies), how
   many images it does not show, and the document route it was rendered with. */
interface Rendered { buf: number; tick: number; name: string; lines: string[]; blocked: number; route: DocPrefixes | null }

const docKey = (d: DocPrefixes | null) => (d ? `${d.root}\u0000${d.base}` : "");
const INSTANT = "instant" as ScrollBehavior;

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
    // A press right of the content is on the scroll bar: dragging it is a pointer scroll like the wheel.
    const down = (ev: PointerEvent) => { if (ev.clientX >= document.documentElement.clientWidth) this.onPointer(); };
    // Image events do not bubble; the capture phase sees every image under the root.
    const loaded = () => this.reanchor();
    const failed = (ev: Event) => { this.imageFailed(ev.target); this.reanchor(); };
    window.addEventListener("scroll", scroll, { passive: true });
    window.addEventListener("wheel", wheel, { passive: true });
    window.addEventListener("pointerdown", down);
    root.addEventListener("load", loaded, true);
    root.addEventListener("error", failed, true);
    // Any height change under the root (a font arriving, the window resized) moves the blocks under the reader.
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(loaded) : null;
    observer?.observe(root);
    this.disposers.push(() => {
      window.removeEventListener("scroll", scroll);
      window.removeEventListener("wheel", wheel);
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
   * the preview while nvim leads; nvim's own view also ends a pointer scroll's lead.
   */
  onView(v: EditorView): void {
    const switched = this.view?.buf !== v.buf;
    const docChanged = this.rendered !== null && this.rendered.buf === v.buf && docKey(this.rendered.route) !== docKey(v.doc);
    this.view = v;
    if (switched) { this.target = null; this.anchor = null; this.pointer = false; this.lastReveal = null; this.revealer.cancel(); }
    const echo = v.cause === "reveal" && v.token !== undefined && this.tokens.has(v.token);
    if (!echo && v.cause === "user") this.pointer = false;
    // nvim moved on its own, so a reveal of a line the page revealed before may be needed again.
    if (!echo) this.lastReveal = v.top;
    if (switched || (!echo && this.lead() === "editor")) {
      this.target = v.top - 1 + v.top_fraction;
      if (this.rendered?.buf === v.buf) this.applyTarget();
      else this.applyOnRenderBuf = v.buf;
    }
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
   */
  onFocus(hasKeys: boolean): void {
    const was = this.lead();
    this.keys = hasKeys;
    this.leadChanged(was);
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
  /** Where the reader reads: one line below the top, so the line at the very top edge, often cut, is not the one. */
  readingY(): number { return window.scrollY + (parseFloat(getComputedStyle(this.root).lineHeight) || 22); }
  /** A scroll the reader asked for. It is not guarded, so it is the reader's like any other and reveals while the
   *  preview leads. */
  scrollByUser(dy: number, smooth: boolean): void { window.scrollBy({ top: dy, behavior: smooth ? "smooth" : INSTANT }); }
  /** Brings an element to the top or a third of the way down; the reader's scroll, so it reveals while leading. */
  scrollToElement(el: Element, at: "top" | "third"): void {
    const y = el.getBoundingClientRect().top + window.scrollY - (at === "third" ? window.innerHeight / 3 : 0);
    window.scrollTo({ top: Math.max(0, y), behavior: "smooth" });
  }
  afterUpdate(fn: (why: UpdateReason) => void): () => void { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; }

  private lead(): Leader { return leader({ sync: this.settings.sync, hasKeys: this.keys, pointerScroll: this.pointer }); }
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
    if (this.lead() === "editor") { if (this.target !== null) this.scrollToLine(this.target); }
    else if (this.anchor !== null) this.scrollToLine(this.anchor);
    this.placeBars();
    this.notify("anchor");
  }

  /* Shows nvim's target at the top. It is also what is shown from now on, so it is the anchor until the reader
     scrolls: a later update or height change while the preview leads keeps it, not a line from before. */
  private applyTarget(): void {
    if (this.target === null) return;
    this.anchor = this.target;
    this.scrollToLine(this.target);
  }

  /* The lead moved: the side taking it starts from the line shown now. */
  private leadChanged(was: Leader): void {
    const now = this.lead();
    if (was === now || this.rendered === null) return;
    if (now === "editor") this.target = this.anchor ?? yToLine(this.index(), window.scrollY);
    else if (was === "editor") this.anchor = yToLine(this.index(), window.scrollY);
  }

  /* The page's own scrolls are instant (a smooth one would be read as the reader moving for its whole length) and
     guarded; a scroll to where the page already is fires no event, so it is not made and the guard not armed. */
  private scrollToLine(L: number): void {
    const index = this.index();
    if (index.length === 0) return;
    const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    const y = lineToY(index, L, max);
    if (Math.abs(y - window.scrollY) >= 1) { this.guard.arm(y); window.scrollTo({ top: y, behavior: INSTANT }); }
    this.traceTop();
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
    if (this.lead() === "preview") this.revealer.request();
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

  /* nvim moves by whole lines, so a reveal goes only when the line at the top changed. */
  private sendReveal(): void {
    if (!this.rendered || this.lead() !== "preview") return;
    const line = Math.max(1, Math.floor(yToLine(this.index(), window.scrollY)) + 1);
    this.traceTop();
    if (line === this.lastReveal) return;
    this.lastReveal = line;
    const token = this.tokens.next();
    trace("reveal", { line, token });
    reveal(this.bridge, { buf: this.rendered.buf, line, token }).catch(() => {});
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
