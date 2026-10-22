import morphdom from "morphdom";

const LINES = ["data-line", "data-line-end"];
/* Source lines move whenever text is typed above an element, and `open` is the reader's: neither makes an element
   different. */
const SKIPPED = new Set(["open", ...LINES]);

/* Equal apart from the skipped attributes, child node by child node. */
function same(a: Element, b: Element): boolean {
  if (a.isEqualNode(b)) return true;
  if (a.tagName !== b.tagName) return false;
  const aa = [...a.attributes].filter((x) => !SKIPPED.has(x.name));
  const ba = [...b.attributes].filter((x) => !SKIPPED.has(x.name));
  if (aa.length !== ba.length || aa.some((x, i) => x.name !== ba[i].name || x.value !== ba[i].value)) return false;
  // Every child node in order, text included: `a **a**` and `**a** a` have the same text and the same elements.
  const ac = a.childNodes, bc = b.childNodes;
  if (ac.length !== bc.length) return false;
  for (let i = 0; i < ac.length; i++) {
    const x = ac[i], y = bc[i];
    if (x.nodeType !== y.nodeType) return false;
    if (x.nodeType === Node.ELEMENT_NODE) { if (!same(x as Element, y as Element)) return false; }
    else if (x.nodeValue !== y.nodeValue) return false;
  }
  return true;
}

/* A kept element still has to say where its source now is, or the sync would map through stale lines. */
function copyLines(from: Element, to: Element): void {
  for (const name of LINES) { const v = to.getAttribute(name); if (v === null) from.removeAttribute(name); else from.setAttribute(name, v); }
  const f = from.querySelectorAll("[data-line]"), t = to.querySelectorAll("[data-line]");
  f.forEach((el, i) => { if (t[i]) for (const name of LINES) { const v = t[i].getAttribute(name); if (v === null) el.removeAttribute(name); else el.setAttribute(name, v); } });
}

/**
 * Puts a new render into the live document by changing only what differs. Equal elements are kept as they are, so
 * the scroll position, loaded images and a `details` the reader opened survive typing elsewhere; a drawn diagram
 * whose key (source and theme) is unchanged is kept so it is not drawn again; a `details` that is replaced keeps the
 * reader's `open`.
 */
export function applyUpdate(live: HTMLElement, fragment: DocumentFragment): void {
  const next = live.cloneNode(false) as HTMLElement;
  next.append(fragment);
  morphdom(live, next, {
    childrenOnly: true,
    onBeforeElUpdated(from, to) {
      const key = from.getAttribute("data-mdp-key");
      if ((key !== null && key === to.getAttribute("data-mdp-key")) || same(from, to)) { copyLines(from, to); return false; }
      if (from.tagName === "DETAILS" && to.tagName === "DETAILS" && from.hasAttribute("open")) to.setAttribute("open", "");
      return true;
    },
  });
}
