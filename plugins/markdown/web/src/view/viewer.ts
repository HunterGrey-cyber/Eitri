import { kit } from "../kit";
import { trace } from "../trace";
import type { Preview } from "./preview";
import {
  carry, fitted, fittedDown, fittedWidth, panBy, panSteps, parseViewBox, toEdge, viewBoxOf,
  zoomBy, zoomSteps, zoomTo, type Size, type View,
} from "./zoom";

/* The diagram viewer: one diagram or image of the document, filling the preview page, moved and zoomed by keys and
   the pointer. It shows a copy of what the page already drew (the sanitised SVG node, or an image element with the
   same `src`, which the page's CSP and image rules already admitted), so nothing is parsed or fetched that the page
   did not. A diagram is redrawn by its `viewBox`, so it is vector-sharp at every zoom; an image is sized, not scaled,
   so the browser resamples it from its own pixels. Nothing of it exists or runs while it is closed. */

export const VIEWER_CONTEXT = "viewer";
const DIAGRAM = "[data-eitri-mermaid]";
const ROOT = "#mdp-root";
/** On the root element while the viewer is open: `chrome.css` hides the page's scroll bar under it. */
export const OPEN_CLASS = "mdp-viewer-open";

/** The keys hint along the viewer's top edge: the reference viewers' words, shortest first. */
export const VIEWER_HINT = "hjkl move · +/- scale · a fit · s width · = 100% · q close";

export const VIEWER_TEXT = {
  nothing: "no diagram or image at the cursor",
  notDrawn: "the diagram is not drawn yet",
  notLoaded: "the image has not loaded",
  gone: "the diagram is no longer in the document",
  imageGone: "the image is no longer in the document",
  stale: "the diagram does not draw now; showing its last drawing",
};

type Kind = "diagram" | "image";
interface Target { kind: Kind; el: Element; key: string; line: number; ordinal: number; total: number }
/* What is shown: the copy, the drawing it was copied from as its key says it (a diagram's `data-mdp-key`, source and
   theme; an image's `src`), its size in picture units and its own viewBox's origin. */
interface Picture { node: SVGSVGElement | HTMLImageElement; drawn: string; size: Size; origin: { x: number; y: number } }

const lineOf = (el: Element): number => {
  const n = Number(el.closest("[data-line]")?.getAttribute("data-line"));
  return Number.isFinite(n) ? n : -1;
};
const rangeOf = (el: Element): [number, number] => {
  const host = el.closest("[data-line]");
  const from = Number(host?.getAttribute("data-line"));
  const to = Number(host?.getAttribute("data-line-end"));
  return [Number.isFinite(from) ? from : -1, Number.isFinite(to) ? to : -1];
};
const keyOf = (kind: Kind, el: Element): string =>
  kind === "diagram" ? el.getAttribute("data-mdp-source") ?? "" : el.getAttribute("src") ?? "";
const drawnSvg = (el: Element): SVGSVGElement | null => el.querySelector<SVGSVGElement>(":scope > svg");
const docTop = (el: Element): number => el.getBoundingClientRect().top + window.scrollY;
const docBottom = (el: Element): number => el.getBoundingClientRect().bottom + window.scrollY;

/** The diagrams or images of the shown document, in document order. */
function candidates(kind: Kind): Element[] {
  const root = document.querySelector(ROOT);
  if (!root) return [];
  return [...root.querySelectorAll(kind === "diagram" ? DIAGRAM : "img")];
}

/** The diagram or image at the cursor: the first one the cursor's band overlaps, in document order (a diagram is a
 *  row of its own; two images on one row give the first), else the first one inside the current block. */
export function targetAtCursor(p: Preview, block: Element | null): Element | null {
  const band = p.cursorBand();
  const root = document.querySelector(ROOT);
  if (!root) return null;
  const all = [...root.querySelectorAll(`${DIAGRAM}, img`)];
  if (band) {
    const hit = all.find((el) => docTop(el) < band.bottom && docBottom(el) > band.top);
    if (hit) return hit;
  }
  if (!block) return null;
  return all.find((el) => block === el || block.contains(el) || el.contains(block)) ?? null;
}

export type ViewerMeasure = { box(stage: HTMLElement): Size };
const domMeasure: ViewerMeasure = { box: (stage) => ({ w: stage.clientWidth, h: stage.clientHeight }) };

/** The single-stroke keys of the kit's own rows that would act on the page under the viewer (scroll it, search it,
 *  go back in its history, select in it): while the viewer is open they are held back. */
const PAGE_ONLY = [
  "scroll.down", "scroll.up", "history.back", "history.forward", "find.start", "find.next", "find.prev", "hint.start",
  "visual.char", "visual.line",
];

/** Whether the key event is the single-stroke key `key` in the kit's notation (`/`, `H`, `<C-e>`). */
export function isStroke(key: string, ev: KeyboardEvent): boolean {
  const ctrl = /^<C-(.)>$/i.exec(key);
  if (ctrl) return ev.ctrlKey && !ev.altKey && !ev.metaKey && ev.key.toLowerCase() === ctrl[1].toLowerCase();
  if ([...key].length !== 1) return false;
  return !ev.ctrlKey && !ev.altKey && !ev.metaKey && ev.key === key;
}

export class Viewer {
  private target: Target | null = null;
  private pic: Picture | null = null;
  private view: View = { scale: 1, x: 0, y: 0 };
  private box: Size = { w: 0, h: 0 };
  /** What the view follows when the picture or the viewer changes size: the whole picture, the whole picture but
   *  never past 100% (an image's opening view), its width, or neither. */
  private fit: "all" | "down" | "width" | null = "all";
  private count = "";
  private overlay: HTMLElement | null = null;
  private stage: HTMLElement | null = null;
  private zoomLabel: HTMLElement | null = null;
  private countLabel: HTMLElement | null = null;
  private teardown: (() => void)[] = [];
  private staleShown = false;
  private buf: number | null = null;

  constructor(private readonly p: Preview, private readonly measure: ViewerMeasure = domMeasure) {}

  isOpen(): boolean { return this.overlay !== null; }
  /** The view, for tests and the trace. */
  current(): View { return { ...this.view }; }
  pending(): string { return this.count; }
  shownElement(): Element | null { return this.target?.el ?? null; }
  /** The document y of the shown diagram's top (where Enter's edit jump lands), or null. */
  targetTop(): number | null { return this.target?.el.isConnected ? docTop(this.target.el) : null; }

  /** `o`: opens the diagram or image at the cursor, fitted to the viewer. A diagram not drawn (still waiting, or an
   *  error), an image not loaded, or nothing there says so on the HUD and opens nothing. */
  openAtCursor(block: Element | null): boolean {
    if (this.isOpen()) return true;
    const el = targetAtCursor(this.p, block);
    if (!el) return this.say(VIEWER_TEXT.nothing);
    const kind: Kind = el.matches(DIAGRAM) ? "diagram" : "image";
    const pic = this.pictureOf(kind, el);
    if (!pic) return this.say(kind === "diagram" ? VIEWER_TEXT.notDrawn : VIEWER_TEXT.notLoaded);
    const all = candidates(kind);
    this.target = { kind, el, key: keyOf(kind, el), line: lineOf(el), ordinal: all.indexOf(el), total: all.length };
    this.buf = this.p.shownBuf();
    this.build();
    this.setPicture(pic);
    this.box = this.measure.box(this.stage!);
    this.fit = this.initialFit();
    this.view = this.initialView(pic.size, this.box);
    this.apply();
    trace("viewer", { state: "open", kind, w: Math.round(pic.size.w), h: Math.round(pic.size.h) });
    return true;
  }

  /** `q`, `<Esc>`: closes the viewer; the page's cursor is back on the diagram's block when an update moved it off. */
  close(): void {
    if (!this.overlay) return;
    const el = this.target?.el ?? null;
    for (const undo of this.teardown.splice(0)) undo();
    this.overlay.remove();
    this.overlay = this.stage = this.zoomLabel = this.countLabel = null;
    this.pic = null;
    this.target = null;
    this.count = "";
    this.staleShown = false;
    trace("viewer", { state: "close" });
    if (el?.isConnected) {
      const band = this.p.cursorBand();
      const top = docTop(el);
      const bottom = docBottom(el);
      if (band && !(band.top < bottom && band.bottom > top)) this.p.jumpCursorTo(top + 0.5);
    }
  }

  /** The page's key actions while the viewer is open; every one is the viewer's (what does not apply does nothing),
   *  so no key acts on the page hidden under it. Counts multiply the moves and zoom steps; `=` takes the count as a
   *  percentage, as nsxiv and zathura do. */
  run(action: string, ev: KeyboardEvent): boolean {
    if (!this.isOpen()) return false;
    if (action === "count" || (action === "viewer.left_edge" && this.count !== "")) {
      const digit = action === "count" ? ev.key : "0";
      if (/^[0-9]$/.test(digit) && this.count.length < 6) this.count += digit;
      this.showCount();
      return true;
    }
    const counted = this.count === "" ? null : Number(this.count);
    const n = counted ?? 1;
    this.count = "";
    this.showCount();
    const pic = this.pic!.size;
    const box = this.box;
    let next: View | null = null;
    let fit: "all" | "width" | null = null;
    switch (action) {
      case "mode.escape":
        // Esc drops a count being typed, as in vim; with none it closes.
        if (counted === null) this.close();
        return true;
      case "viewer.close": this.close(); return true;
      case "cursor.down": next = panSteps(this.view, 0, n, pic, box); break;
      case "cursor.up": next = panSteps(this.view, 0, -n, pic, box); break;
      case "viewer.left": next = panSteps(this.view, -n, 0, pic, box); break;
      case "viewer.right": next = panSteps(this.view, n, 0, pic, box); break;
      case "cursor.half_down": next = panBy(this.view, 0, (n * box.h) / 2, pic, box); break;
      case "cursor.half_up": next = panBy(this.view, 0, (-n * box.h) / 2, pic, box); break;
      case "cursor.page_down": next = panBy(this.view, 0, n * box.h, pic, box); break;
      case "cursor.page_up": next = panBy(this.view, 0, -n * box.h, pic, box); break;
      case "cursor.first": next = toEdge(this.view, "top", pic, box); break;
      case "cursor.last": next = toEdge(this.view, "bottom", pic, box); break;
      case "viewer.left_edge": next = toEdge(this.view, "left", pic, box); break;
      case "viewer.right_edge": next = toEdge(this.view, "right", pic, box); break;
      case "viewer.zoom_in": next = zoomSteps(this.view, n, pic, box); break;
      case "viewer.zoom_out": next = zoomSteps(this.view, -n, pic, box); break;
      case "viewer.actual":
        next = zoomTo(this.view, (counted ?? 100) / 100, box.w / 2, box.h / 2, pic, box);
        break;
      case "viewer.fit": next = fitted(pic, box); fit = "all"; break;
      case "viewer.fit_width": next = fittedWidth(pic, box); fit = "width"; break;
      default: return true;
    }
    this.fit = fit;
    this.setView(next);
    return true;
  }

  /* A diagram opens fitted (the page already shows it scaled down to the column; the viewer is for seeing it larger);
     an image opens fitted only when it is larger than the viewer, else at 100%, as nsxiv opens one. */
  private initialFit(): "all" | "down" {
    return this.target?.kind === "image" ? "down" : "all";
  }

  private initialView(pic: Size, box: Size): View {
    return this.initialFit() === "down" ? fittedDown(pic, box) : fitted(pic, box);
  }

  private say(text: string): boolean {
    kit()?.message(text);
    return false;
  }

  private pictureOf(kind: Kind, el: Element): Picture | null {
    if (kind === "diagram") {
      const svg = drawnSvg(el);
      if (!svg || el.classList.contains("mdp-diagram-pending")) return null;
      const vb = parseViewBox(svg.getAttribute("viewBox"));
      const rect = svg.getBoundingClientRect();
      const size = vb ? { w: vb.w, h: vb.h } : { w: rect.width, h: rect.height };
      if (!(size.w > 0 && size.h > 0)) return null;
      // A copy of the node the page drew from the sanitised fragment: never source parsed again. It keeps its
      // `<style>` and ids (its rules and markers name them) and fills the stage; the view is its `viewBox`.
      const node = svg.cloneNode(true) as SVGSVGElement;
      node.removeAttribute("style");
      node.setAttribute("width", "100%");
      node.setAttribute("height", "100%");
      node.setAttribute("preserveAspectRatio", "xMinYMin meet");
      node.setAttribute("aria-hidden", "true");
      return { node, drawn: el.getAttribute("data-mdp-key") ?? "", size, origin: vb ? { x: vb.x, y: vb.y } : { x: 0, y: 0 } };
    }
    const img = el as HTMLImageElement;
    if (!img.complete || !(img.naturalWidth > 0 && img.naturalHeight > 0)) return null;
    const node = document.createElement("img");
    // The same `src` the page's image rules resolved and its CSP admitted.
    node.setAttribute("src", img.getAttribute("src") ?? "");
    node.alt = img.alt;
    node.draggable = false;
    return { node, drawn: img.getAttribute("src") ?? "", size: { w: img.naturalWidth, h: img.naturalHeight }, origin: { x: 0, y: 0 } };
  }

  private build(): void {
    const overlay = document.createElement("div");
    overlay.className = "mdp-viewer";
    overlay.setAttribute("role", "dialog");
    overlay.setAttribute("aria-label", "diagram viewer");
    const stage = document.createElement("div");
    stage.className = "mdp-viewer-stage";
    const hint = document.createElement("div");
    hint.className = "mdp-viewer-hint";
    const keys = document.createElement("span");
    keys.className = "mdp-viewer-keys";
    keys.textContent = VIEWER_HINT;
    const count = document.createElement("span");
    count.className = "mdp-viewer-count";
    const zoom = document.createElement("span");
    zoom.className = "mdp-viewer-zoom";
    const close = document.createElement("button");
    close.type = "button";
    close.className = "mdp-viewer-close";
    close.tabIndex = -1;
    close.setAttribute("aria-label", "close the viewer");
    close.textContent = "×";
    hint.append(keys, count, zoom, close);
    overlay.append(stage, hint);
    document.body.append(overlay);
    // The page under the viewer shows no scroll bar (and the wheel cannot scroll it) until the viewer closes.
    document.documentElement.classList.add(OPEN_CLASS);
    this.teardown.push(() => document.documentElement.classList.remove(OPEN_CLASS));
    this.overlay = overlay;
    this.stage = stage;
    this.zoomLabel = zoom;
    this.countLabel = count;
    this.listen(close, "mousedown", (ev) => ev.preventDefault());
    this.listen(close, "click", () => this.close());
    this.installPointer(stage);
    this.installGuard();
    // The document updating under the viewer (an edit in nvim, a theme's redraw, a diagram finishing): the viewer
    // stays on the same diagram and redraws it.
    const off = this.p.afterUpdate((why) => {
      if (why === "render" || why === "anchor" || why === "notice") this.resync();
    });
    this.teardown.push(off);
    if (typeof ResizeObserver === "function") {
      const ro = new ResizeObserver(() => this.resize());
      ro.observe(stage);
      this.teardown.push(() => ro.disconnect());
    }
  }

  private listen<K extends keyof HTMLElementEventMap>(
    el: HTMLElement | Window, type: K, fn: (ev: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions,
  ): void {
    el.addEventListener(type, fn as EventListener, opts);
    this.teardown.push(() => el.removeEventListener(type, fn as EventListener, opts));
  }

  /* The kit's own rows run without asking the page; those that would act on the hidden page are held back here,
     in the capture phase, before the kit sees them. */
  private installGuard(): void {
    const onKey = (ev: KeyboardEvent) => {
      const k = kit();
      if (!k || ev.isComposing) return;
      if (!PAGE_ONLY.some((action) => k.keys(action).some((key) => isStroke(key, ev)))) return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
    };
    this.listen(window, "keydown", onKey, { capture: true });
  }

  /* The wheel moves, Ctrl+wheel (a touchpad's pinch) zooms at the pointer, a drag moves, a double-click toggles
     between the fitted view and twice that, at the pointer. */
  private installPointer(stage: HTMLElement): void {
    const at = (ev: MouseEvent) => {
      const r = stage.getBoundingClientRect();
      return { x: ev.clientX - r.left, y: ev.clientY - r.top };
    };
    this.listen(stage, "wheel", (ev) => {
      ev.preventDefault();
      if (!this.pic) return;
      const unit = ev.deltaMode === 1 ? 40 : ev.deltaMode === 2 ? this.box.h : 1;
      if (ev.ctrlKey) {
        const { x, y } = at(ev);
        this.fit = null;
        this.setView(zoomBy(this.view, Math.exp((-ev.deltaY * unit) / 300), x, y, this.pic.size, this.box));
        return;
      }
      const dx = ev.shiftKey && ev.deltaX === 0 ? ev.deltaY : ev.deltaX;
      const dy = ev.shiftKey && ev.deltaX === 0 ? 0 : ev.deltaY;
      this.fit = null;
      this.setView(panBy(this.view, dx * unit, dy * unit, this.pic.size, this.box));
    }, { passive: false });
    let drag: { id: number; x: number; y: number } | null = null;
    this.listen(stage, "pointerdown", (ev) => {
      if (ev.button !== 0) return;
      drag = { id: ev.pointerId, x: ev.clientX, y: ev.clientY };
      stage.setPointerCapture?.(ev.pointerId);
      stage.classList.add("mdp-viewer-dragging");
      ev.preventDefault();
    });
    this.listen(stage, "pointermove", (ev) => {
      if (!drag || ev.pointerId !== drag.id || !this.pic) return;
      const dx = drag.x - ev.clientX;
      const dy = drag.y - ev.clientY;
      drag = { ...drag, x: ev.clientX, y: ev.clientY };
      this.fit = null;
      this.setView(panBy(this.view, dx, dy, this.pic.size, this.box));
    });
    const end = (ev: PointerEvent) => {
      if (!drag || ev.pointerId !== drag.id) return;
      drag = null;
      stage.releasePointerCapture?.(ev.pointerId);
      stage.classList.remove("mdp-viewer-dragging");
    };
    this.listen(stage, "pointerup", end);
    this.listen(stage, "pointercancel", end);
    this.listen(stage, "dblclick", (ev) => {
      ev.preventDefault();
      if (!this.pic) return;
      const size = this.pic.size;
      const first = this.initialView(size, this.box);
      if (Math.abs(this.view.scale / first.scale - 1) < 1e-3) {
        const { x, y } = at(ev);
        this.fit = null;
        this.setView(zoomTo(this.view, 2 * first.scale, x, y, size, this.box));
      } else {
        this.fit = this.initialFit();
        this.setView(first);
      }
    });
  }

  private setPicture(pic: Picture): void {
    this.pic = pic;
    this.stage!.replaceChildren(pic.node);
    this.stage!.classList.toggle("mdp-viewer-image", pic.node instanceof HTMLImageElement);
  }

  private setView(v: View): void {
    this.view = v;
    this.apply();
  }

  /* Draws the view: a diagram's `viewBox`, an image's size and place (in whole screen px, so it is never a scaled
     bitmap of a bitmap). */
  private apply(): void {
    const pic = this.pic;
    if (!pic) return;
    const v = this.view;
    if (pic.node instanceof HTMLImageElement) {
      Object.assign(pic.node.style, {
        width: `${pic.size.w * v.scale}px`, height: `${pic.size.h * v.scale}px`,
        left: `${-v.x * v.scale}px`, top: `${-v.y * v.scale}px`,
      });
    } else {
      pic.node.setAttribute("viewBox", viewBoxOf(v, this.box, pic.origin));
    }
    if (this.zoomLabel) this.zoomLabel.textContent = `${Math.round(v.scale * 100)}%`;
  }

  private showCount(): void {
    if (this.countLabel) this.countLabel.textContent = this.count;
  }

  private resize(): void {
    if (!this.pic || !this.stage) return;
    const box = this.measure.box(this.stage);
    if (box.w === this.box.w && box.h === this.box.h) return;
    this.view = this.carried(this.pic.size, this.pic.size, this.box, box);
    this.box = box;
    this.apply();
  }

  private carried(from: Size, to: Size, fromBox: Size, toBox: Size): View {
    if (this.fit === "width") return fittedWidth(to, toBox);
    if (this.fit === "down") return fittedDown(to, toBox);
    return carry(this.view, this.fit === "all", from, to, fromBox, toBox);
  }

  /** Finds the shown diagram (or image) again after an update: one with the same source (or `src`), the nearest to
   *  its last line; else the same element, if the update kept it; else the one in the same place among them when
   *  their number did not change; else the one whose lines hold its last line. */
  private find(): Element | null {
    const t = this.target!;
    const all = candidates(t.kind);
    const same = all.filter((el) => keyOf(t.kind, el) === t.key);
    if (same.length) {
      return same.reduce((a, b) => (Math.abs(lineOf(b) - t.line) < Math.abs(lineOf(a) - t.line) ? b : a));
    }
    if (all.includes(t.el)) return t.el;
    if (all.length === t.total && t.ordinal >= 0 && t.ordinal < all.length) return all[t.ordinal];
    return all.find((el) => {
      const [from, to] = rangeOf(el);
      return from <= t.line && t.line < to;
    }) ?? null;
  }

  private resync(): void {
    const t = this.target;
    if (!t || !this.pic) return;
    // Another buffer's document is never taken for this one's, whatever it holds.
    if (this.p.shownBuf() === null || this.p.shownBuf() !== this.buf || document.querySelector(ROOT)?.hasAttribute("hidden")) {
      this.closeGone();
      return;
    }
    const el = this.find();
    if (!el) {
      this.closeGone();
      return;
    }
    const all = candidates(t.kind);
    this.target = { kind: t.kind, el, key: keyOf(t.kind, el), line: lineOf(el), ordinal: all.indexOf(el), total: all.length };
    // The update kept the drawing (an unchanged key): nothing to redraw. The page's own SVG node is no guide, since
    // an update may reshape it in place into another drawing.
    if (t.kind === "image" && el.getAttribute("src") === this.pic.drawn) return;
    if (t.kind === "diagram") {
      if (el.getAttribute("data-mdp-key") === this.pic.drawn && drawnSvg(el)) return;
      // Waiting to be drawn again (an edit, a theme): the last drawing stays until the new one is there.
      if (el.classList.contains("mdp-diagram-pending")) return;
      if (!drawnSvg(el)) {
        if (!this.staleShown) kit()?.message(VIEWER_TEXT.stale);
        this.staleShown = true;
        return;
      }
    }
    const pic = this.pictureOf(t.kind, el);
    if (!pic) return;
    this.staleShown = false;
    const from = this.pic.size;
    this.setPicture(pic);
    this.view = this.carried(from, pic.size, this.box, this.box);
    this.apply();
    trace("viewer", { state: "redraw", w: Math.round(pic.size.w), h: Math.round(pic.size.h) });
  }

  private closeGone(): void {
    const kind = this.target?.kind;
    this.target = null;
    this.close();
    kit()?.message(kind === "image" ? VIEWER_TEXT.imageGone : VIEWER_TEXT.gone);
  }
}

