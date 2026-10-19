/*! The prefix-free hint label scheme below is adapted from Vimium, under this notice (also ../LICENSE-vimium):
 *
 * The MIT License (MIT)
 *
 * Copyright (c) 2010 Phil Crosby, Ilya Sukhar.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
import type { HintTarget } from "./index";

export type { HintTarget } from "./index";

const TARGETS = 'a[href], button, summary, input, select, textarea, [role="button"], [role="tab"], ' +
  '[role="option"], [role="checkbox"], [role="switch"], [role="menuitem"], [data-eitri-target]';
const CLIPPING = /^(auto|scroll|hidden|clip|overlay)$/;

function visibleRect(el: Element, root: Document) {
  const view = root.defaultView;
  if (!view || !el.isConnected || el.ownerDocument !== root || el.matches(":disabled")) return null;
  // A modal dialog makes the rest of the document inert without adding an inert attribute.
  for (const dialog of root.querySelectorAll("dialog[open]")) {
    let modal = false;
    try { modal = dialog.matches(":modal"); } catch { /* Older DOM implementations have no modal selector. */ }
    if (modal && !dialog.contains(el)) return null;
  }
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return null;
  let left = Math.max(0, rect.left);
  let top = Math.max(0, rect.top);
  let right = Math.min(view.innerWidth, rect.right);
  let bottom = Math.min(view.innerHeight, rect.bottom);
  for (let ancestor: Element | null = el; ancestor; ancestor = ancestor.parentElement) {
    // An inert subtree takes no input from the user, so a label must not reach it through `click()` either.
    if (ancestor.getAttribute("aria-hidden") === "true" || ancestor.getAttribute("aria-disabled") === "true" ||
      ancestor.hasAttribute("hidden") || ancestor.hasAttribute("inert")) return null;
    const style = view.getComputedStyle(ancestor);
    if (style.display === "none") return null;
    // Visibility is inherited but a descendant can explicitly make itself visible again.
    if (ancestor === el && (style.visibility === "hidden" || style.visibility === "collapse")) return null;
    if (ancestor === el) continue;
    const clipX = CLIPPING.test(style.overflowX || style.overflow);
    const clipY = CLIPPING.test(style.overflowY || style.overflow);
    if (!clipX && !clipY) continue;
    const bounds = ancestor.getBoundingClientRect();
    const { offsetWidth, offsetHeight } = ancestor as HTMLElement;
    // Client dimensions use layout coordinates; the bounding rectangle includes ancestor scaling.
    const scaleX = offsetWidth ? bounds.width / offsetWidth : 1;
    const scaleY = offsetHeight ? bounds.height / offsetHeight : 1;
    // The client box excludes the border and scrollbar, which cannot show a clipped descendant.
    const clipLeft = bounds.left + ancestor.clientLeft * scaleX;
    const clipTop = bounds.top + ancestor.clientTop * scaleY;
    if (clipX) {
      left = Math.max(left, clipLeft);
      right = Math.min(right, clipLeft + ancestor.clientWidth * scaleX);
    }
    if (clipY) {
      top = Math.max(top, clipTop);
      bottom = Math.min(bottom, clipTop + ancestor.clientHeight * scaleY);
    }
  }
  return right > left && bottom > top ? { left, top, right, bottom } : null;
}

export function collectTargets(root: Document, extra: HintTarget[]): HintTarget[] {
  const targets = new Map<Element, HintTarget>();
  for (const el of root.querySelectorAll(`${TARGETS}, [tabindex]`)) {
    if (el.matches(TARGETS) || (el as HTMLElement).tabIndex >= 0) targets.set(el, { el });
  }
  for (const target of extra) targets.set(target.el, target);
  return [...targets.values()].filter(({ el }) => visibleRect(el, root) !== null);
}

export function assignLabels(
  targets: HintTarget[], origin: { x: number; y: number }, alphabet = "sadfjklewcmpgh",
): Map<Element, string> {
  if (!targets.length) return new Map();
  const characters = [...new Set(alphabet)];
  if (characters.length < 2) throw new Error("Hint labels need at least two different characters");
  const distance = (el: Element) => {
    const rect = el.getBoundingClientRect();
    return (rect.left + rect.width / 2 - origin.x) ** 2 + (rect.top + rect.height / 2 - origin.y) ** 2;
  };
  const ordered = targets.map((target) => ({ ...target, distance: distance(target.el) }));
  ordered.sort((a, b) => ((a.rank ?? Infinity) - (b.rank ?? Infinity)) || a.distance - b.distance);

  // Expanding a leaf replaces it with its children, so none of the remaining labels is a prefix.
  const labels = [""];
  let offset = 0;
  while (labels.length - offset < ordered.length || labels.length === 1) {
    const prefix = labels[offset++];
    for (const character of characters) labels.push(prefix + character);
  }
  const selected = labels.slice(offset, offset + ordered.length).sort((a, b) => a.length - b.length);
  return new Map(ordered.map(({ el }, index) => [el, selected[index]]));
}

export function activate(el: Element): void {
  const target = el as HTMLElement;
  target.focus?.();
  target.click?.();
}

export function startHints(root: Document, extra: HintTarget[], origin: { x: number; y: number }) {
  const labels = assignLabels(collectTargets(root, extra), origin);
  const overlay = root.createElement("div");
  overlay.id = "eitri-keys-hints";
  overlay.setAttribute("aria-hidden", "true");
  overlay.style.cssText = "position:fixed;inset:0;pointer-events:none;z-index:2147483646";
  const markers = new Map<Element, HTMLElement>();
  for (const [el, label] of labels) {
    const rect = visibleRect(el, root)!;
    const marker = root.createElement("span");
    marker.className = "eitri-hint";
    marker.textContent = label;
    marker.style.cssText = `position:fixed;left:${rect.left}px;top:${rect.top}px;padding:1px 3px;` +
      "background:#ffe082;color:#171717;border:1px solid #171717;border-radius:2px;" +
      "font:bold 13px monospace;line-height:16px;text-transform:none";
    if (el.hasAttribute("data-eitri-danger")) {
      marker.classList.add("eitri-hint-danger");
      marker.style.background = "#9c2525";
      marker.style.color = "#fff";
    }
    overlay.append(marker);
    markers.set(el, marker);
  }
  let active = labels.size > 0;
  let prefix = "";
  // Targets that have left the viewport since the labels were drawn: their labels are hidden and cannot be typed.
  const gone = new Set<Element>();
  if (active) root.body.append(overlay);
  const cancel = () => { active = false; overlay.remove(); };
  const show = () => {
    let matched = false;
    for (const [el, label] of labels) {
      const visible = !gone.has(el) && label.startsWith(prefix);
      markers.get(el)!.hidden = !visible;
      matched ||= visible;
    }
    if (!matched) cancel();
  };
  return {
    get active() { return active; },
    get prefix() { return prefix; },
    cancel,
    /** Moves each label to where its target is now; called when the page scrolls or the viewport changes. */
    reposition() {
      if (!active) return;
      // Every position is read before any label moves, so the page is laid out once.
      const rects = [...labels.keys()].map((el) => [el, visibleRect(el, root)] as const);
      gone.clear();
      for (const [el, rect] of rects) {
        if (!rect) { gone.add(el); continue; }
        const marker = markers.get(el)!;
        marker.style.left = `${rect.left}px`;
        marker.style.top = `${rect.top}px`;
      }
      show();
    },
    feed(key: string): Element | null {
      if (!active) return null;
      if (key === "Escape") { cancel(); return null; }
      prefix += key.length === 1 ? key.toLowerCase() : key;
      for (const [el, label] of labels) {
        if (label === prefix && !gone.has(el)) { cancel(); return el; }
      }
      show();
      return null;
    },
  };
}
