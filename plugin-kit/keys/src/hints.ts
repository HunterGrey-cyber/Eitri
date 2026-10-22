import { onRemove } from "./bridge";
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
import { ensureStyle } from "./style";
import { isOperable } from "./visible";

export type { HintTarget } from "./index";

export type Rect = { left: number; top: number; right: number; bottom: number };

function intersects(a: Rect, b: Rect): boolean {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

export function placeLabel(
  target: Rect, size: { width: number; height: number }, view: { width: number; height: number },
  avoid: readonly Rect[], gap = 2,
): { left: number; top: number; side: "left" | "above" | "below" | "right" | "inside" } {
  const { width, height } = size;
  // On a tall target the label sits on its first line, as a list marker does.
  const line = target.top + Math.max(0, Math.min(target.bottom - target.top, 2 * height) - height) / 2;
  const sides = [
    ["left", target.left - gap - width, line],
    ["above", Math.max(0, target.left), target.top - gap - height],
    ["below", Math.max(0, target.left), target.bottom + gap],
    ["right", target.right + gap, line],
  ] as const;
  for (const [side, left, top] of sides) {
    const box = { left, top, right: left + width, bottom: top + height };
    const fits = box.left >= 0 && box.top >= 0 && box.right <= view.width && box.bottom <= view.height;
    if (fits && !avoid.some((other) => intersects(box, other))) return { left, top, side };
  }
  // A target reaching under the HUD or past the edge still gets a whole label inside the view.
  const left = Math.max(0, Math.min(Math.max(target.left, target.right - width), view.width - width));
  const top = Math.max(0, Math.min(target.top, view.height - height));
  return { left, top, side: "inside" };
}

let kept: { el: Element; label: string } | null = null;

onRemove(() => { kept = null; });

const TARGETS = 'a[href], button, summary, input, select, textarea, [role="button"], [role="tab"], ' +
  '[role="option"], [role="checkbox"], [role="switch"], [role="menuitem"], [data-eitri-target]';
const CLIPPING = /^(auto|scroll|hidden|clip|overlay)$/;

function visibleRect(el: Element, root: Document) {
  const view = root.defaultView;
  if (!view || !el.isConnected || el.ownerDocument !== root || !isOperable(el)) return null;
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
  keep?: { el: Element; label: string } | null,
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
  const retained = keep && targets.some(({ el }) => el === keep.el) ? keep : null;
  const eligible = (label: string) => !retained ||
    (!label.startsWith(retained.label) && !retained.label.startsWith(label));
  const labels = [""];
  let offset = 0;
  let available = eligible("") ? 1 : 0;
  const needed = ordered.length - (retained ? 1 : 0);
  while (available < needed || labels.length === 1) {
    const prefix = labels[offset++];
    // The reserved label owns its entire subtree; only its ancestors need expanding.
    if (retained && prefix.startsWith(retained.label)) continue;
    if (eligible(prefix)) available--;
    for (const character of characters) {
      const label = prefix + character;
      labels.push(label);
      if (eligible(label)) available++;
    }
  }
  const selected = labels.slice(offset).filter(eligible).slice(0, needed).sort((a, b) => a.length - b.length);
  let index = 0;
  return new Map(ordered.map(({ el }) => [el, retained?.el === el ? retained.label : selected[index++]]));
}

export function activate(el: Element): void {
  const target = el as HTMLElement;
  target.focus?.();
  target.click?.();
}

export function startHints(
  root: Document, extra: HintTarget[], origin: { x: number; y: number }, alphabet = "sadfjklewcmpgh",
) {
  const targets = collectTargets(root, extra);
  if (kept && !targets.some(({ el }) => el === kept!.el)) kept = null;
  const labels = assignLabels(targets, origin, alphabet, kept);
  ensureStyle(root);
  const overlay = root.createElement("div");
  overlay.id = "eitri-keys-hints";
  overlay.setAttribute("aria-hidden", "true");
  overlay.style.cssText = "position:fixed;inset:0;pointer-events:none;z-index:2147483646";
  const markers = new Map<Element, HTMLElement>();
  for (const [el, label] of labels) {
    const marker = root.createElement("span");
    marker.className = "eitri-hint";
    marker.textContent = label;
    if (el.hasAttribute("data-eitri-danger")) marker.classList.add("eitri-hint-danger");
    overlay.append(marker);
    markers.set(el, marker);
  }
  let active = labels.size > 0;
  let prefix = "";
  // Targets that have left the viewport since the labels were drawn: their labels are hidden and cannot be typed.
  const gone = new Set<Element>();
  if (active) root.body.append(overlay);
  const sizes = new Map([...markers].map(([el, marker]) => {
    const { width, height } = marker.getBoundingClientRect();
    return [el, { width, height }] as const;
  }));
  const place = () => {
    // Every position is read before any label moves, so the page is laid out once.
    const rects = [...labels.keys()].map((el) => [el, visibleRect(el, root)] as const);
    const hud = root.getElementById("eitri-keys-hud");
    const hudHeight = hud && !hud.hidden ? hud.getBoundingClientRect().height : 0;
    const view = { width: root.defaultView!.innerWidth, height: root.defaultView!.innerHeight - hudHeight };
    const placed: Rect[] = [];
    gone.clear();
    for (const [el, rect] of rects) {
      if (!rect) { gone.add(el); continue; }
      const avoid = rects.flatMap(([other, bounds]) => other !== el && bounds ? [bounds] : []);
      const size = sizes.get(el)!;
      const { left, top } = placeLabel(rect, size, view, [...avoid, ...placed]);
      const marker = markers.get(el)!;
      marker.style.left = `${left}px`;
      marker.style.top = `${top}px`;
      placed.push({ left, top, right: left + size.width, bottom: top + size.height });
    }
  };
  if (active) place();
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
      place();
      show();
    },
    feed(key: string): Element | null {
      if (!active) return null;
      if (key === "Escape") { cancel(); return null; }
      if (key === "Backspace") {
        if (prefix) { prefix = prefix.slice(0, -1); show(); }
        else cancel();
        return null;
      }
      prefix += key.length === 1 ? key.toLowerCase() : key;
      for (const [el, label] of labels) {
        if (label === prefix && !gone.has(el)) {
          kept = el.hasAttribute("data-eitri-hint-keep") ? { el, label } : null;
          cancel();
          return el;
        }
      }
      show();
      return null;
    },
  };
}
