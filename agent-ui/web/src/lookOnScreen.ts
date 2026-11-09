/** The panel's layers that draw over other content but take no pointer (`pointer-events: none`), so a hit test looks
 *  straight through them: any of them present, shown, and meeting the look's boxes (inside its own clipping
 *  ancestors) counts as covering the look. `indexCss.test.ts` keeps this list whole: every `pointer-events: none`
 *  rule in `index.css` names one of these, but `.band-measure`, which is never painted. */
export const PASS_THROUGH_LAYERS: readonly string[] = [".hint-layer", ".cursor-band", ".visual-paint", ".dash-entry-slot"];

type Box = { left: number; top: number; right: number; bottom: number };

/** Whether every character of `look` inside `el`'s text is drawn where a person can see it: measured from the
 *  layout, never read from the markup. The text is found in `el`'s text nodes; each box its characters occupy
 *  (`Range.getClientRects`) must have a size and lie inside the viewport and inside every ancestor that clips its
 *  overflow (`el` itself included: an ellipsis or a too-narrow box hides characters past its edge). The top element
 *  must be `el` or inside it at the start, middle and end of every box and at the first and last character (the end
 *  of a host's own domain), and no pass-through layer (`PASS_THROUGH_LAYERS`) may meet any box. An engine without
 *  range geometry (`getClientRects` absent, or no boxes at all) measures nothing visible. */
export function lookOnScreen(el: Element, look: string): boolean {
  if (look === "") return false;
  const doc = el.ownerDocument;
  const view = doc.defaultView;
  if (view === null) return false;
  const nodes: Text[] = [];
  const walker = doc.createTreeWalker(el, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) nodes.push(n as Text);
  const at = nodes.map((n) => n.data).join("").indexOf(look);
  if (at < 0) return false;
  const place = (offset: number, end: boolean) => {
    let seen = 0;
    for (const n of nodes) {
      if (offset < seen + n.data.length || (end && offset === seen + n.data.length)) return { node: n, offset: offset - seen };
      seen += n.data.length;
    }
    return null;
  };
  /** The boxes of the characters from `from` to `to` (offsets into `el`'s text), or `null` with no range geometry. */
  const boxesOf = (from: number, to: number): DOMRect[] | null => {
    const start = place(from, false);
    const end = place(to, true);
    if (start === null || end === null) return null;
    const range = doc.createRange();
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
    if (typeof range.getClientRects !== "function") return null;
    return Array.from(range.getClientRects());
  };
  const boxes = boxesOf(at, at + look.length);
  if (boxes === null || boxes.length === 0) return false;
  // The look's first and last character on their own: the last is where a long host's own domain ends.
  const first = boxesOf(at, at + 1) ?? [];
  const last = boxesOf(at + look.length - 1, at + look.length) ?? [];
  const vw = doc.documentElement.clientWidth || view.innerWidth;
  const vh = doc.documentElement.clientHeight || view.innerHeight;
  const clipsOf = (from: Element): Box[] => {
    const clips: Box[] = [];
    for (let a: Element | null = from; a !== null && a !== doc.documentElement; a = a.parentElement) {
      const style = view.getComputedStyle(a);
      if (style.overflowX !== "visible" || style.overflowY !== "visible") clips.push(a.getBoundingClientRect());
    }
    return clips;
  };
  const clips = clipsOf(el);
  const inside = (r: Box, c: Box) =>
    r.left >= c.left - 0.5 && r.top >= c.top - 0.5 && r.right <= c.right + 0.5 && r.bottom <= c.bottom + 0.5;
  const shows = (x: number, y: number) => {
    const hit = doc.elementFromPoint(x, y);
    return hit !== null && (hit === el || el.contains(hit));
  };
  for (const r of boxes) {
    if (r.width <= 0 || r.height <= 0) return false;
    if (!inside(r, { left: 0, top: 0, right: vw, bottom: vh })) return false;
    if (clips.some((c) => !inside(r, c))) return false;
    const y = r.top + r.height / 2;
    const edge = Math.min(1, r.width / 2);
    if (!shows(r.left + edge, y) || !shows(r.left + r.width / 2, y) || !shows(r.right - edge, y)) return false;
  }
  for (const r of [...first, ...last]) {
    if (r.width <= 0 || r.height <= 0 || !shows(r.left + r.width / 2, r.top + r.height / 2)) return false;
  }
  // Layers a hit test looks through: one that is shown where any box is covers the look.
  const meets = (a: Box, b: Box) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
  for (const selector of PASS_THROUGH_LAYERS) {
    for (const layer of Array.from(doc.querySelectorAll(selector))) {
      if (layer === el || el.contains(layer) || layer.contains(el)) continue;
      const style = view.getComputedStyle(layer);
      if (style.display === "none" || style.visibility === "hidden") continue;
      let box: Box = layer.getBoundingClientRect();
      for (const c of clipsOf(layer.parentElement ?? layer)) {
        box = { left: Math.max(box.left, c.left), top: Math.max(box.top, c.top), right: Math.min(box.right, c.right), bottom: Math.min(box.bottom, c.bottom) };
      }
      if (box.right <= box.left || box.bottom <= box.top) continue;
      if (boxes.some((r) => meets(r, box))) return false;
    }
  }
  return true;
}
