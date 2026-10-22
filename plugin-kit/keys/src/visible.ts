/** How deep the search below a `display: contents` element goes; past it the element's place in the page decides. */
const CONTENTS_DEPTH = 8;

/** Whether the engine draws a box for `el`: `checkVisibility` where the engine has it, else an element with no
 *  client rects is not drawn. */
function drawn(el: Element): boolean {
  if (typeof (el as HTMLElement).checkVisibility === "function") return (el as HTMLElement).checkVisibility();
  return el.getClientRects().length > 0;
}

/** Whether the place a `display: contents` element occupies is drawn: its text sits in the box of its nearest
 *  ancestor that has one, so that ancestor decides. */
function placeDrawn(el: Element): boolean {
  for (let at = el.parentElement; at; at = at.parentElement) {
    if (getComputedStyle(at).display !== "contents") return drawn(at);
  }
  return true;
}

function holdsText(el: Element): boolean {
  return [...el.childNodes].some((node) => node.nodeType === Node.TEXT_NODE && /\S/.test(node.textContent ?? ""));
}

/** Whether CSS renders `el` at all: a `display: none` ancestor (a folded contents group) leaves it with no box, and
 *  focusing it fails, which would trap list movement on it. An element that is itself `display: contents` has no box
 *  either way but its content is drawn in its place, so it counts as rendered when one of its element children is,
 *  or when it holds text of its own (or the search runs out of depth) and its place is drawn. A wrapper that holds
 *  no text and whose children are all undrawn shows nothing. */
function renderedAt(el: Element, depth: number): boolean {
  if (getComputedStyle(el).display !== "contents") return drawn(el);
  if (depth >= CONTENTS_DEPTH) return placeDrawn(el);
  if ([...el.children].some((child) => renderedAt(child, depth + 1))) return true;
  return holdsText(el) && placeDrawn(el);
}

export function isRendered(el: Element): boolean {
  return renderedAt(el, 0);
}

/** Input must not reach a control the browser disables or a subtree hidden from interaction. A `disabled` attribute
 *  on an element the browser does not disable (a link, say) leaves it working, so it does not count here. */
export function isOperable(el: Element): boolean {
  return !el.matches(":disabled") && !el.closest("[inert]") &&
    !el.closest('[hidden], [aria-hidden="true"]') &&
    getComputedStyle(el).visibility !== "hidden" && isRendered(el);
}
