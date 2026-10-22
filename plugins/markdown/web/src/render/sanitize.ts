import createDOMPurify, { type DOMPurify } from "dompurify";
import { NONCE_ATTR, type BlockReason, type DocPrefixes } from "./env";
import { resolveImage } from "./images";
import { blockedText } from "../view/notices";

export interface SanitizeContext {
  nonce: string;
  doc: DocPrefixes | null;
  remoteImages: "https" | "off";
  /** Every image the sanitiser replaced with a box, in document order, for the HUD's count. */
  blocked: { reason: BlockReason; alt: string }[];
}

/* What a forge shows from raw HTML in a README, plus `section` (footnotes are wrapped in one) and `input` (the task
 * checkboxes, which survive only when the renderer made them). */
const ALLOWED_TAGS = [
  "h1", "h2", "h3", "h4", "h5", "h6", "p", "br", "hr", "b", "i", "strong", "em", "s", "del", "ins", "sub", "sup",
  "mark", "small", "kbd", "samp", "var", "abbr", "q", "code", "pre", "a", "img", "div", "span", "ol", "ul", "li",
  "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "th", "td", "caption", "blockquote", "details",
  "summary", "figure", "figcaption", "ruby", "rt", "rp", "section", "input",
];
const ALLOWED_ATTR = ["href", "src", "alt", "title", "align", "colspan", "rowspan", "width", "height", "open", "start",
  "type", "cite", "datetime", "id", "name"];
/* Attributes a document may write only on one kind of element: elsewhere they would size, open or name things the
 * page's own code reads. A map, not an object: the document names the attributes looked up here, and an object
 * would answer `constructor` or `__proto__` from its prototype. */
const ONLY_ON = new Map<string, string[]>([
  ["width", ["img"]], ["height", ["img"]], ["open", ["details"]], ["start", ["ol"]], ["type", ["ol"]], ["name", ["a"]],
]);
/* Attributes only the renderer may write: they decide styling, sync and what the page's own code acts on, so a
 * document writing them could restyle the page or steer the scroll sync. */
const PRIVILEGED = new Set(["class", "data-line", "data-line-end", "disabled", "checked", NONCE_ATTR]);
/* `http`, `https`, `mailto`, relative paths and fragments; any other scheme fails. DOMPurify strips whitespace from
 * the value before testing it, so a padded `javascript:` fails too. */
const URI = /^(?:(?:https?|mailto):|[^a-z]|[a-z][a-z0-9+.\-]*(?:[^a-z0-9+.\-:]|$))/i;

/* The state of the one `sanitize` call in progress. The hooks belong to the instance, not the call, so they read it
 * from here; it is set only for the duration of that call. */
interface Run {
  ctx: SanitizeContext;
  /* Elements carrying this render's nonce: what the renderer wrote, as against what the document wrote. */
  marked: WeakSet<Element>;
  /* The document's own images, resolved after the walk. */
  images: { img: Element; src: string }[];
  /* Form controls to remove after the walk. */
  inputs: Element[];
}

let run: Run | null = null;
let documentInstance: DOMPurify | null = null;
let svgInstance: DOMPurify | null = null;

/* Made on first use rather than at import, so loading the module needs no window. */
function documentPurifier(): DOMPurify {
  if (documentInstance) return documentInstance;
  const p = createDOMPurify(window);
  // Attributes are visited last first, so whether the element carries the nonce is read before any of them is
  // touched: removing the nonce first would make the element's other privileged attributes look forged. An empty
  // nonce marks nothing: a document can write an empty value as easily as no attribute.
  p.addHook("beforeSanitizeAttributes", (node) => {
    if (!run || run.ctx.nonce === "" || !(node instanceof Element)) return;
    if (node.getAttribute(NONCE_ATTR) === run.ctx.nonce) run.marked.add(node);
  });
  p.addHook("uponSanitizeAttribute", (node, data) => {
    if (!run) {
      data.keepAttr = false;
      return;
    }
    const tag = node.tagName.toLowerCase();
    const name = data.attrName;
    const marked = run.marked.has(node);
    if (PRIVILEGED.has(name) || name.startsWith("data-eitri-")) {
      // Kept exactly as written, past DOMPurify's own checks, which would drop every one of them.
      if (marked) data.forceKeepAttr = true;
      else data.keepAttr = false;
      return;
    }
    if (marked && tag === "input" && name === "type") {
      // The renderer makes only checkboxes; any other control would take input the page never reads.
      if (data.attrValue === "checkbox") data.forceKeepAttr = true;
      else data.keepAttr = false;
      return;
    }
    if (marked && tag === "img" && name === "src") {
      // The renderer already resolved this source against the document's project.
      data.forceKeepAttr = true;
      return;
    }
    const only = ONLY_ON.get(name);
    if (only && !only.includes(tag)) {
      data.keepAttr = false;
      return;
    }
    if (name === "src") {
      // A document's image is resolved the way a Markdown image is, after the walk; until then it has no source,
      // so nothing loads from what the document wrote. No other element may load anything.
      if (tag === "img") run.images.push({ img: node, src: data.attrValue });
      data.keepAttr = false;
      return;
    }
    if ((name === "id" || name === "name") && !marked) {
      // A document's id would otherwise become a global the page's code could read in place of its own. Rewriting
      // the value here means DOMPurify's own check of named properties sees the prefixed value.
      data.attrValue = `user-content-${data.attrValue}`;
    }
  });
  p.addHook("afterSanitizeAttributes", (node) => {
    if (!run || !(node instanceof Element)) return;
    if (node.tagName === "INPUT" && !(run.marked.has(node) && node.getAttribute("type") === "checkbox")) {
      run.inputs.push(node);
    }
  });
  documentInstance = p;
  return p;
}

function box(img: Element, reason: BlockReason, alt: string): Element {
  const d = img.ownerDocument;
  const outer = d.createElement("span");
  outer.className = "mdp-box";
  outer.setAttribute("data-eitri-blocked", reason);
  const why = d.createElement("span");
  why.className = "mdp-box-reason";
  why.textContent = blockedText(reason);
  outer.append(why);
  if (alt !== "") outer.append(` ${alt}`);
  return outer;
}

/**
 * The document's HTML, as the renderer wrote it with the document's raw HTML inside, each piece of raw HTML followed
 * by `RAW_HTML_END` (without it, raw HTML that stops inside a tag takes over the renderer's next tag), made safe to
 * put into the page: what the renderer marked with this render's nonce keeps its privileged attributes, raw HTML keeps a forge's
 * element set with its images resolved and its ids prefixed, and nothing that runs, frames, posts or restyles
 * survives. Images that are not loaded become boxes and are added to `ctx.blocked`.
 */
export function sanitizeDocument(html: string, ctx: SanitizeContext): DocumentFragment {
  const p = documentPurifier();
  const current: Run = { ctx, marked: new WeakSet(), images: [], inputs: [] };
  run = current;
  let fragment: DocumentFragment;
  try {
    fragment = p.sanitize(html, {
      ALLOWED_TAGS, ALLOWED_ATTR, ALLOW_DATA_ATTR: false, ALLOW_ARIA_ATTR: false, ALLOWED_URI_REGEXP: URI,
      RETURN_DOM_FRAGMENT: true, KEEP_CONTENT: true,
    });
  } finally {
    run = null;
  }
  // The fragment holds the very nodes DOMPurify walked, so what the hooks recorded is changed here. Changing it
  // during the walk would have the new nodes walked too, and a box's classes stripped.
  for (const el of current.inputs) el.remove();
  for (const { img, src } of current.images) {
    if (!fragment.contains(img)) continue;
    const r = resolveImage(src, ctx.doc, ctx.remoteImages);
    if ("url" in r) {
      img.setAttribute("src", r.url);
      continue;
    }
    const alt = img.getAttribute("alt") ?? "";
    ctx.blocked.push({ reason: r.blocked, alt });
    img.replaceWith(box(img, r.blocked, alt));
  }
  // The nonce has done its work. Each render draws a new one, so leaving it would make every element differ from
  // the previous render's and the live update rewrite them all.
  for (const el of fragment.querySelectorAll(`[${NONCE_ATTR}]`)) el.removeAttribute(NONCE_ATTR);
  return fragment;
}

/**
 * A diagram's SVG, from the diagram library, made safe to put into the page. It has an instance of its own: the
 * document instance's hooks would strip every class the diagram's own stylesheet relies on. Nothing in it may run,
 * embed HTML, load an image, link, or reference another element.
 */
export function sanitizeSvg(svg: string): DocumentFragment {
  if (!svgInstance) svgInstance = createDOMPurify(window);
  // `feImage` loads an image or references an element just as `image` and `use` do.
  return svgInstance.sanitize(svg, {
    USE_PROFILES: { svg: true, svgFilters: true }, ADD_TAGS: ["style"],
    FORBID_TAGS: ["foreignObject", "image", "use", "a", "script", "feImage"], RETURN_DOM_FRAGMENT: true,
  });
}
