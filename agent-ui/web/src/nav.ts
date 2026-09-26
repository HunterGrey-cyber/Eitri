/**
 * Keyboard navigation follows the panel's document structure. `hjkl` uses known DOM order,
 * avoiding guesses about the nearest control based on pixel positions.
 *
 * A stop carries `data-nav-stop`: a conversation row, banner, status line or terminal-handoff
 * area, or a choice row or mode button on the start screen. `j`/`k` walk stops in document order.
 * Stops without usable controls are skipped except for rows, which hold the conversation cursor
 * for `Enter`/`y` even when they have no button.
 *
 * `h`/`l` walk enabled buttons, inputs, textareas and radios within a stop. Document order applies
 * unless `data-nav-order` overrides it. Permission cards put Approve before Deny and the reason
 * box so the most frequent action is one keypress away.
 *
 * The focused control is the selected control. `Tab`, clicks and `h`/`l` move the same DOM focus,
 * and `Enter`/`Space` activate it natively, so separate selection state cannot disagree with focus.
 */

export const STOP_ATTR = "data-nav-stop";

/** Label alphabet shared with the window-wide `f` HINT. It excludes `f` because that key starts
 *  HINT. The `gf` path picker reuses it for its letter-per-path footer list. */
export const HINT_ALPHABET = "asdjklghweruio";

const CONTROL_SELECTOR =
  'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), [role="radio"]:not([aria-disabled="true"])';

/** Moves `index` by `delta` within `[min, length - 1]`, clamped rather than wrapped. Holding `j` at
 *  the bottom of a list must not jump to the top: that loses your place. */
export function clampStep(length: number, index: number, delta: number, min = 0): number {
  if (length === 0) return min;
  return Math.max(min, Math.min(length - 1, index + delta));
}

/** The enabled controls inside `stop`, left to right. A stop that is itself a control (a start
 *  screen mode button, a choice row) is its own only control. */
export function controlsOf(stop: HTMLElement): HTMLElement[] {
  if (stop.matches(CONTROL_SELECTOR)) return [stop];
  const found = Array.from(stop.querySelectorAll<HTMLElement>(CONTROL_SELECTOR));
  const order = (el: HTMLElement) => Number(el.dataset.navOrder ?? Number.POSITIVE_INFINITY);
  // A stable sort, so controls without `data-nav-order` keep document order among themselves.
  return found
    .map((el, i) => ({ el, i }))
    .sort((a, b) => order(a.el) - order(b.el) || a.i - b.i)
    .map(({ el }) => el);
}

/** Every stop under `root` worth landing on, in document order. */
export function stopsIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(`[${STOP_ATTR}]`)).filter(
    (stop) => stop.getAttribute(STOP_ATTR) === "row" || controlsOf(stop).length > 0,
  );
}

/** The stop that currently holds the keyboard: the one containing the focused element, if focus
 *  is inside one; otherwise the conversation row at `cursor`, if there is one. `null` on a start
 *  screen before anything has been selected. */
export function currentStop(root: HTMLElement, cursor: number | null): HTMLElement | null {
  const active = document.activeElement;
  if (active instanceof HTMLElement && active !== root && root.contains(active)) {
    const stop = active.closest<HTMLElement>(`[${STOP_ATTR}]`);
    if (stop !== null) return stop;
  }
  if (cursor === null) return null;
  const rows = Array.from(root.querySelectorAll<HTMLElement>(`[${STOP_ATTR}="row"]`));
  return rows[cursor] ?? null;
}

/** Where `j` (+1) or `k` (-1) goes from the current stop. With nothing current (a start screen
 *  nobody has moved on yet), both land on the first stop. */
export function nextStop(root: HTMLElement, cursor: number | null, delta: 1 | -1): HTMLElement | null {
  const stops = stopsIn(root);
  if (stops.length === 0) return null;
  const current = currentStop(root, cursor);
  const index = current === null ? -1 : stops.indexOf(current);
  if (index === -1) return stops[0];
  return stops[clampStep(stops.length, index, delta)];
}

/** The index of `stop` among the conversation rows, or `null` if it is not a row. */
export function rowIndexOf(root: HTMLElement, stop: HTMLElement): number | null {
  if (stop.getAttribute(STOP_ATTR) !== "row") return null;
  const rows = Array.from(root.querySelectorAll<HTMLElement>(`[${STOP_ATTR}="row"]`));
  const index = rows.indexOf(stop);
  return index === -1 ? null : index;
}

/**
 * Where `h` (-1) or `l` (+1) goes inside `stop`. Returns the control to focus, or `"stop"` to hand
 * focus back to the stop itself.
 *
 * Only a row can be "back to the stop", because only a row has a cursor to return to: from its
 * first control, `h` leaves the controls and the row cursor holds the keys again. Every other stop
 * is reached by focusing a control, so its first control is as far left as it goes.
 */
export function nextControl(stop: HTMLElement, delta: 1 | -1): HTMLElement | "stop" | null {
  const controls = controlsOf(stop);
  if (controls.length === 0) return null;
  const active = document.activeElement;
  const index = active instanceof HTMLElement ? controls.indexOf(active) : -1;
  const isRow = stop.getAttribute(STOP_ATTR) === "row";
  if (index === -1) return delta === 1 ? controls[0] : isRow ? null : controls[0];
  if (index === 0 && delta === -1) return isRow ? "stop" : controls[0];
  return controls[clampStep(controls.length, index, delta)];
}

/** One conversation item, as far as `a`/`d` need to know. */
export type AnswerableItem =
  | { kind: "permission"; toolUseId: string | null }
  | { kind: "tool"; toolUseId: string }
  | { kind: "other" };

/** The timeline index of the permission card `a`/`d` should answer from `cursor`, or `null`. The
 * cursor must be on the card or the tool call it gates. From a tool row, choose the matching
 * `toolUseId` among the permission items directly following it. Other rows have no target,
 * preventing a shortcut from approving an unrelated call. */
export function permissionTarget(items: AnswerableItem[], cursor: number): number | null {
  const here = items[cursor];
  if (here === undefined) return null;
  if (here.kind === "permission") return cursor;
  if (here.kind !== "tool" || here.toolUseId === "") return null;
  for (let i = cursor + 1; i < items.length; i++) {
    const item = items[i];
    if (item.kind === "permission" && item.toolUseId === here.toolUseId) return i;
    if (item.kind !== "permission") break;
  }
  return null;
}

/** The card `a`/`d` answer from `cursor`: use `permissionTarget` first, then the sole card if
 *  the conversation has exactly one. With multiple cards, keep the cursor rule so a shortcut
 *  cannot guess which waiting call to answer. */
export function answerTarget(items: AnswerableItem[], cursor: number): number | null {
  const own = permissionTarget(items, cursor);
  if (own !== null) return own;
  const cards = items.flatMap((item, index) => (item.kind === "permission" ? [index] : []));
  return cards.length === 1 ? cards[0] : null;
}

/** A panel target of global `f` HINT: a conversation row, a code block inside a row, an enabled
 *  control from `controlsOf`, or a marked composer. Code targets remember their row so landing
 *  moves the row cursor. The list is frozen at `hint_collect`, and `shell` uses entry indices.
 *  `rowIndex` is only the index at collection time. Landing reads it from the element again
 *  because rows can be inserted above it while labels are displayed. */
export type HintTarget =
  | { kind: "row"; el: HTMLElement; rowIndex: number }
  | { kind: "control"; el: HTMLElement }
  | { kind: "code"; el: HTMLElement; rowIndex: number }
  | { kind: "composer"; el: HTMLElement };

/** Marks the composer as a HINT target. The composer is in no
 *  `data-nav-stop` -- `j`/`k` never walk into it, `i` and `Ctrl+l` are its routes -- and in BROWSE it
 *  is a plain `div`, which `CONTROL_SELECTOR` does not match, so without this marker HINT could never
 *  reach it. `Composer` sets it only while INPUT can be entered. */
export const HINT_COMPOSER_ATTR = "data-hint-composer";

/** The elements known to scroll their own content (index.css): the conversation's list, the tab
 *  bar, and the launch chooser's list. Named here as
 *  well as read off the computed style below, because jsdom loads no stylesheet and the tests must
 *  see these three. `.session-choice` is gone -- the old resumable-session picker it named was
 *  replaced by `EmptyTab` (F3) and the launch chooser (`.chooser-list`). */
const SCROLL_CONTAINERS = ".message-list, .tab-bar, .chooser-list";

function clips(el: HTMLElement): boolean {
  if (el.matches(SCROLL_CONTAINERS)) return true;
  const style = getComputedStyle(el);
  return [style.overflow, style.overflowX, style.overflowY].some((v) => v !== "" && v !== "visible");
}

function intersects(a: DOMRect, b: DOMRect): boolean {
  return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
}

/** Whether `el` shows inside `root`: its non-zero box must intersect the root and every ancestor
 *  that clips overflow. Checking every clipping ancestor prevents nested scrollers from exposing
 *  targets that an outer scroller hides. */
export function hintVisible(el: HTMLElement, root: HTMLElement): boolean {
  const box = el.getBoundingClientRect();
  if (box.width === 0 || box.height === 0) return false;
  for (let a = el.parentElement; a !== null && a !== root; a = a.parentElement) {
    if (clips(a) && !intersects(box, a.getBoundingClientRect())) return false;
  }
  return intersects(box, root.getBoundingClientRect());
}

/** Visible HINT targets under `root`, in document order: for each stop (row or other), the stop
 *  itself (if it's a row), then its code blocks, then its controls; for a non-row stop, its
 *  controls only. The composer, when marked (`HINT_COMPOSER_ATTR`), takes its place in document
 *  order among the stops. "Visible" is `hintVisible` above. */
export function hintTargets(root: HTMLElement): HintTarget[] {
  const targets: HintTarget[] = [];
  const composerEl = root.querySelector<HTMLElement>(`[${HINT_COMPOSER_ATTR}]`);
  let composer = composerEl !== null && hintVisible(composerEl, root) ? composerEl : null;
  for (const stop of stopsIn(root)) {
    if (composer !== null && composer.compareDocumentPosition(stop) & Node.DOCUMENT_POSITION_FOLLOWING) {
      targets.push({ kind: "composer", el: composer });
      composer = null;
    }
    const rowIndex = stop.getAttribute(STOP_ATTR) === "row" ? rowIndexOf(root, stop) : null;
    if (rowIndex !== null && hintVisible(stop, root)) targets.push({ kind: "row", el: stop, rowIndex });
    if (rowIndex !== null) {
      for (const block of stop.querySelectorAll<HTMLElement>("pre.code-block")) {
        if (hintVisible(block, root)) targets.push({ kind: "code", el: block, rowIndex });
      }
    }
    for (const control of controlsOf(stop)) {
      if (hintVisible(control, root)) targets.push({ kind: "control", el: control });
    }
  }
  if (composer !== null) targets.push({ kind: "composer", el: composer });
  return targets;
}
