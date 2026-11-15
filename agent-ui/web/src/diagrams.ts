import createDOMPurify from "dompurify";
import { codeBlockText } from "./markdown";
import { isHiddenChar, revealedText } from "./revealHidden";

export const MAX_SVG_BYTES = 512 * 1024;
export const MAX_DRAWINGS = 64;
export const MAX_DOCUMENT_SVG_BYTES = 16 * 1024 * 1024;
const SELECTOR = "pre.code-block[data-code-diagram]";
export type DiagramRequest = { type: "render_diagram"; id: string; source: string };
export type DiagramReply = { id: string; svg?: string; error?: string; later?: boolean; transient?: boolean; disabled?: boolean; startup?: boolean };
type Drawing = { url: string; width: number; height: number; bytes: number };
type Outcome = { drawing: Drawing } | { error: string };
const bytes = (text: string) => new TextEncoder().encode(text).length;

function decodedCss(value: string): string {
  return value.replace(/\/\*[\s\S]*?\*\//g, "").replace(
    /\\([0-9a-f]{1,6})(?:\r\n|[ \t\r\n\f])?|\\([^\r\n\f])|\\(?:\r\n|[\r\n\f])/gi,
    (_whole, hex: string | undefined, char: string | undefined) => {
      if (!hex) return char ?? "";
      const point = parseInt(hex, 16);
      return String.fromCodePoint(point === 0 || point > 0x10ffff || (point >= 0xd800 && point <= 0xdfff) ? 0xfffd : point);
    },
  ).toLowerCase();
}

/** A stylesheet with any outside reference is discarded as a unit, including nested rules. */
function outsideCss(value: string): boolean {
  const css = decodedCss(value);
  if (/(?:image-set|-webkit-image-set|image|cross-fade|element|src)\s*\(|@import/.test(css)) return true;
  const withoutLocalUrls = css.replace(/url\s*\(([^)]*)\)/g, (whole, target: string) => {
    const local = target.trim().replace(/^(['"])([\s\S]*)\1$/, "$2").trim();
    return local.startsWith("#") ? "" : whole;
  });
  return /url\s*\(/.test(withoutLocalUrls);
}

/** CSS inside an SVG image still needs its own filter: HTML sanitizers do not parse stylesheet text. */
function safeCss(value: string): string {
  // Escapes can disguise both function and property names. Mermaid's generated styles need none.
  if (value.includes("\\")) return "";
  let css = value.replace(/\/\*[\s\S]*?\*\//g, "");
  // No at-rule is needed in a static drawing. Drop complete nested blocks as well as semicolon forms.
  for (let at = css.indexOf("@"); at !== -1; at = css.indexOf("@")) {
    let end = at + 1;
    let depth = 0;
    let quoted = "";
    for (; end < css.length; end++) {
      const ch = css[end];
      if (quoted) { if (ch === quoted) quoted = ""; continue; }
      if (ch === "'" || ch === '"') { quoted = ch; continue; }
      if (ch === "{") depth++;
      if (ch === "}" && --depth <= 0) { end++; break; }
      if (ch === ";" && depth === 0) { end++; break; }
    }
    css = css.slice(0, at) + css.slice(end);
  }
  css = css.replace(/url\s*\(([^)]*)\)/gi, (_whole, target: string) => {
    const local = target.trim().replace(/^(['"])(.*)\1$/, "$2");
    return /^#[\w.:-]+$/.test(local) ? `url(${local})` : "none";
  });
  // Unclosed functions are invalid CSS; discard rather than relying on error recovery.
  if (/url\s*\(/i.test(css.replace(/url\(#[\w.:-]+\)/g, ""))) return "";
  css = css.replace(/(^|[;{])[^;{}]*\b(?:image-set|image)\s*\([^;}]*/gi, "$1");
  return css.replace(/(^|[;{}])\s*(?:-[a-z]+-)?animation[\w-]*\s*:[^;}]*/gi, "$1");
}

let purifier: ReturnType<typeof createDOMPurify> | null = null;
function sanitize(svg: string): Drawing | null {
  const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") return null;
  if (!purifier) {
    purifier = createDOMPurify(window);
    purifier.addHook("uponSanitizeElement", (node) => {
      if (node.nodeName.toLowerCase() === "style") {
        const css = node.textContent ?? "";
        if (outsideCss(css)) node.parentNode?.removeChild(node);
        else node.textContent = safeCss(css);
      }
    });
    purifier.addHook("uponSanitizeAttribute", (_node, data) => {
      const value = data.attrValue.replace(/\/\*[\s\S]*?\*\//g, "");
      const unsafeUrl = /url\s*\(/i.test(value.replace(/url\s*\(\s*(['"]?)#[\w.:-]+\1\s*\)/gi, ""));
      if ((/^(?:href|xlink:href)$/i.test(data.attrName) && !data.attrValue.startsWith("#")) ||
          /(?:image-set|image|cross-fade|element|src)\s*\(/i.test(value) || unsafeUrl || value.includes("\\")) {
        data.keepAttr = false;
      } else if (data.attrName === "style") data.attrValue = safeCss(data.attrValue);
    });
  }
  const fragment = purifier.sanitize(parsed.documentElement, {
    USE_PROFILES: { svg: true, svgFilters: true }, RETURN_DOM_FRAGMENT: true, ADD_TAGS: ["style"],
    FORBID_TAGS: ["foreignObject", "image", "feImage", "use", "a", "script", "animate", "animateTransform", "animateMotion", "set"],
  });
  const clean = fragment.querySelector("svg");
  if (!clean) return null;
  const box = (clean.getAttribute("viewBox") ?? "").trim().split(/[\s,]+/).map(Number);
  if (box.length !== 4 || !box.every(Number.isFinite) || box[2] <= 0 || box[3] <= 0) return null;
  clean.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  const xml = new XMLSerializer().serializeToString(clean);
  if (bytes(xml) > MAX_SVG_BYTES) return null;
  return { url: "data:image/svg+xml," + encodeURIComponent(xml), width: box[2], height: box[3], bytes: bytes(xml) };
}

/** One document's bounded drawing cache. Only reply bodies enroll roots; other markdown never attaches. */
export class DiagramController {
  private enabled = false;
  private generation = 0;
  private serial = 0;
  private frozen = false;
  private pruneScheduled = false;
  private roots = new Set<HTMLElement>();
  private cache = new Map<string, Outcome>();
  private budgetRefusals = new Map<string, Outcome>();
  private drawingCache = new Map<string, Drawing>();
  private pendingKeys = new Set<string>();
  private deferred = new Map<string, Outcome>();
  private observed = new Map<HTMLElement, HTMLElement>();
  private pending = new Map<string, { key: string; source: string; generation: number }>();
  private disabled = new Set<string>();
  private failures = new Map<string, { count: number; stopped: boolean }>();
  private drawings = new Map<HTMLElement, Drawing>();
  private observer: IntersectionObserver | null = null;
  send: (message: DiagramRequest) => void;
  constructor(send: (message: DiagramRequest) => void, private connected = (el: HTMLElement) => el.isConnected) { this.send = send; }

  configure(enabled: boolean, generation = this.generation): void {
    const suppressed = this.disabled.size > 0;
    this.disabled.clear();
    this.configureState(enabled, generation, suppressed);
  }

  private configureState(enabled: boolean, generation: number, force = false): void {
    if (this.enabled === enabled && this.generation === generation && !force) return;
    const changed = this.generation !== generation;
    this.enabled = enabled;
    this.generation = generation;
    if (changed) {
      this.failures.clear();
      this.deferred.clear();
      this.budgetRefusals.clear();
      for (const [key, outcome] of this.cache) if ("error" in outcome && !key.startsWith(`${generation}\0`)) this.cache.delete(key);
    }
    this.attachAll();
  }

  configureDiagramTheme(generation: number): void { this.configureState(this.enabled, generation); }

  freeze(frozen: boolean): void {
    if (this.frozen === frozen) return;
    this.frozen = frozen;
    if (!frozen) {
      this.attachAll();
      this.deferred.clear();
    }
  }

  private attachAll(): void {
    if (!this.enabled || this.frozen) return;
    for (const root of this.liveRoots()) this.attachRoot(root);
  }

  private capacityFreed(): void {
    // A refusal lasts until document capacity changes, even if the cache itself could be trimmed.
    this.budgetRefusals.clear();
  }

  private unwatch(el: HTMLElement): void {
    if (this.observed.delete(el)) this.observer?.unobserve(el);
  }

  private pruneRoots(): void {
    for (const root of this.roots) if (!this.connected(root)) this.roots.delete(root);
    for (const el of this.observed.keys()) if (!this.connected(el)) this.unwatch(el);
  }

  private pruneDrawings(): void {
    let freed = false;
    for (const el of this.drawings.keys()) if (!this.connected(el)) { this.drawings.delete(el); freed = true; }
    if (freed) this.capacityFreed();
  }

  private liveRoots(): HTMLElement[] {
    this.pruneRoots();
    this.pruneDrawings();
    return [...this.roots];
  }

  detach(root: HTMLElement): void {
    this.roots.delete(root);
    let freed = false;
    for (const el of this.drawings.keys()) if (root.contains(el)) { this.drawings.delete(el); freed = true; }
    if (freed) this.capacityFreed();
    for (const el of this.observed.keys()) if (root.contains(el)) this.unwatch(el);
  }

  private visible(el: HTMLElement): boolean {
    const box = el.getBoundingClientRect();
    const clip = el.closest(".message-list")?.getBoundingClientRect();
    return box.bottom >= Math.max(0, clip?.top ?? 0) && box.top <= Math.min(window.innerHeight, clip?.bottom ?? window.innerHeight);
  }

  attach(root: HTMLElement): void {
    this.roots.add(root);
    if (!this.enabled || this.frozen) return;
    // DOM writes between attaches must release drawing capacity before the next fence is considered.
    this.pruneDrawings();
    if (!this.pruneScheduled) {
      this.pruneRoots();
      this.pruneScheduled = true;
      queueMicrotask(() => { this.pruneScheduled = false; });
    }
    this.attachRoot(root);
  }

  private refuseBudget(key: string, error: string): Outcome {
    const outcome = { error };
    this.budgetRefusals.set(key, outcome);
    const drawing = this.drawingCache.get(key);
    if (drawing && ![...this.drawings.values()].includes(drawing)) {
      this.cache.delete(key); this.drawingCache.delete(key);
    }
    return outcome;
  }

  private attachRoot(root: HTMLElement): void {
    root.querySelectorAll<HTMLElement>(SELECTOR).forEach((el) => {
      const source = codeBlockText(el);
      const key = `${this.generation}\0${source}`;
      if (this.disabled.has(source)) { this.showCode(el); return; }
      const refusal = this.budgetRefusals.get(key);
      const held = refusal ?? this.cache.get(key) ?? this.deferred.get(key);
      const previous = [...this.drawingCache].reverse().find(([oldKey]) => oldKey.slice(oldKey.indexOf("\0") + 1) === source)?.[1];
      if (previous && !refusal && (!held || "error" in held)) this.apply(el, { drawing: previous });
      if (held) { this.unwatch(el); this.apply(el, held); return; }
      let characters = source.length;
      for (let at = 0; at < source.length; at++) if (source.codePointAt(at)! > 0xffff) { characters--; at++; }
      if (characters > 20000) {
        this.apply(el, { error: `too long for the panel (${characters} characters; the limit is 20,000)` }); return;
      }
      if ([...source].some(isHiddenChar)) { this.apply(el, { error: "it holds hidden characters" }); return; }
      if (source.split("\n").length > 500) { this.apply(el, { error: "too many source lines (the limit is 500)" }); return; }
      if (!this.drawings.has(el) && this.drawings.size >= MAX_DRAWINGS) {
        this.apply(el, this.refuseBudget(key, "this conversation already shows 64 diagrams")); return;
      }
      if (this.pendingKeys.has(key)) { this.unwatch(el); return; }
      if (!this.visible(el)) {
        if (typeof IntersectionObserver !== "undefined") {
          this.observer ??= new IntersectionObserver((entries) => {
            if (!this.enabled || this.frozen) return;
            this.liveRoots();
            const roots = new Set(entries.filter((entry) => entry.isIntersecting)
              .map((entry) => this.observed.get(entry.target as HTMLElement)).filter((root) => root !== undefined));
            for (const root of roots) this.attachRoot(root);
          });
          if (!this.observed.has(el)) { this.observer.observe(el); this.observed.set(el, root); }
        }
        return;
      }
      this.unwatch(el);
      const id = `diagram-${++this.serial}`;
      this.pending.set(id, { key, source, generation: this.generation });
      this.pendingKeys.add(key);
      this.send({ type: "render_diagram", id, source });
    });
  }

  receive(reply: DiagramReply): void {
    const request = this.pending.get(reply.id);
    if (!request) return;
    this.pending.delete(reply.id);
    this.pendingKeys.delete(request.key);
    if (request.generation !== this.generation) return;
    const roots = this.liveRoots();
    if (reply.disabled) {
      this.disabled.add(request.source);
      if (this.frozen) return;
      for (const root of roots) root.querySelectorAll<HTMLElement>(SELECTOR).forEach((el) => {
        if (codeBlockText(el) !== request.source) return;
        this.showCode(el);
      });
      return;
    }
    let outcome: Outcome;
    if (typeof reply.svg === "string") {
      if (bytes(reply.svg) > MAX_SVG_BYTES) outcome = { error: "the drawing is too large" };
      else {
        const drawing = sanitize(reply.svg);
        outcome = drawing ? { drawing } : { error: "the drawing could not be shown" };
      }
    } else outcome = { error: reply.later ? "this diagram type needs a later version" : (reply.error ?? "the renderer stopped") };
    let cacheable = !reply.transient && !reply.startup;
    if (!reply.startup && (reply.error === "took longer than 4 s" || reply.error === "the renderer stopped")) {
      const previous = this.failures.get(request.key);
      const failure = { count: (previous?.count ?? 0) + 1, stopped: previous?.stopped === true || reply.error === "the renderer stopped" };
      this.failures.set(request.key, failure);
      while (this.failures.size > 64) this.failures.delete(this.failures.keys().next().value!);
      cacheable = failure.count >= 2;
      if (cacheable && failure.stopped) outcome = { error: "the renderer stopped on it twice" };
    }
    if (cacheable) {
      if ("drawing" in outcome) {
        for (const oldKey of this.drawingCache.keys()) if (oldKey.slice(oldKey.indexOf("\0") + 1) === request.source) {
          this.cache.delete(oldKey); this.drawingCache.delete(oldKey);
        }
        this.drawingCache.set(request.key, outcome.drawing);
      }
      this.cache.set(request.key, outcome);
      let drawings = [...this.drawingCache.values()];
      let size = drawings.reduce((n, drawing) => n + drawing.bytes, 0);
      const pinned = new Set(this.drawings.values());
      while (drawings.length > MAX_DRAWINGS || size > MAX_DOCUMENT_SVG_BYTES) {
        const first = [...this.drawingCache].find(([key, drawing]) => key !== request.key && !pinned.has(drawing));
        if (!first) break;
        const [key, value] = first;
        size -= value.bytes;
        this.cache.delete(key);
        this.drawingCache.delete(key);
        drawings = [...this.drawingCache.values()];
      }
      if ("drawing" in outcome && (drawings.length > MAX_DRAWINGS || size > MAX_DOCUMENT_SVG_BYTES)) {
        outcome = this.refuseBudget(request.key, size > MAX_DOCUMENT_SVG_BYTES
          ? "this conversation already reaches the 16 MiB SVG limit" : "this conversation already shows 64 diagrams");
      }
    } else if (this.frozen) this.deferred.set(request.key, outcome);
    if (this.frozen) return;
    for (const root of roots) root.querySelectorAll<HTMLElement>(SELECTOR).forEach((el) => {
      if (codeBlockText(el) === request.source) this.apply(el, outcome);
    });
  }

  private showCode(el: HTMLElement): void {
    this.unwatch(el);
    el.querySelector(".diagram-error")?.remove();
    el.querySelector(".diagram-scroll")?.remove();
    el.classList.remove("diagram-drawn");
    if (this.drawings.delete(el)) this.capacityFreed();
  }

  private apply(el: HTMLElement, outcome: Outcome): void {
    if ("drawing" in outcome) {
      const drawing = outcome.drawing;
      const old = this.drawings.get(el);
      if (old?.url === drawing.url) return;
      const total = [...this.drawings.values()].reduce((n, d) => n + d.bytes, 0) - (old?.bytes ?? 0) + drawing.bytes;
      const key = `${this.generation}\0${codeBlockText(el)}`;
      if (!old && this.drawings.size >= MAX_DRAWINGS) { this.apply(el, this.refuseBudget(key, "this conversation already shows 64 diagrams")); return; }
      if (total > MAX_DOCUMENT_SVG_BYTES) { this.apply(el, this.refuseBudget(key, "this conversation already reaches the 16 MiB SVG limit")); return; }
      const img = document.createElement("img");
      img.alt = "Diagram (mermaid)";
      img.width = drawing.width;
      img.height = drawing.height;
      img.style.minWidth = `${drawing.width * .6}px`;
      img.src = drawing.url;
      const scroll = document.createElement("span");
      scroll.className = "diagram-scroll";
      scroll.append(img);
      el.querySelector(".diagram-scroll")?.remove();
      el.querySelector(".diagram-error")?.remove();
      el.append(scroll);
      el.classList.add("diagram-drawn");
      this.drawings.set(el, drawing);
    } else {
      // An older theme's drawing stays until a replacement succeeds.
      if (el.classList.contains("diagram-drawn")) return;
      let note = el.querySelector<HTMLElement>(".diagram-error");
      if (!note) { note = document.createElement("span"); note.className = "diagram-error"; el.append(note); }
      const first = Array.from(outcome.error.split(/[\r\n]/)[0]).slice(0, 200).join("");
      note.textContent = "Diagram not drawn: " + revealedText(first);
    }
  }
}

export const panelDiagrams = new DiagramController(() => {});
export const attachDiagrams = (root: HTMLElement) => panelDiagrams.attach(root);
export const detachDiagrams = (root: HTMLElement) => panelDiagrams.detach(root);
