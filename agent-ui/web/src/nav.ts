/**
 * Keyboard reach for every control in the panel, by structure rather than by geometry
 * (2026-09-19). The owner wanted every button reachable "直观，而且符合直觉", and chose `hjkl` over
 * geometric spatial navigation after the trade-off was laid out: the panel is a vertical document,
 * so its structure is known exactly, whereas "the nearest button in that direction" would have to
 * be inferred from pixels and would surprise.
 *
 * The model:
 *
 * - A **stop** is anything carrying `data-nav-stop`: a conversation row (`"row"`), a banner, the
 *   status line, the terminal-handoff area; on the start screen, each choice row and each mode
 *   button. `j`/`k` walk stops in document order, top to bottom. A stop with no usable control is
 *   skipped, except a row: rows carry the conversation cursor and are worth landing on for
 *   `Enter`/`y` even when they hold no button.
 * - A stop's **controls** are its enabled buttons, inputs, textareas and radios. `h`/`l` walk them
 *   left to right. Order is document order unless a control sets `data-nav-order`, which the
 *   permission card uses to put Approve before Deny before the reason box: the most frequent action
 *   first, one keypress away.
 * - The **selected control is simply the focused one.** No parallel selection state that could
 *   disagree with DOM focus: `Tab`, a click and `h`/`l` all move the same thing, and `Enter`/`Space`
 *   activate it natively.
 */

export const STOP_ATTR = "data-nav-stop";

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

/**
 * The timeline index of the permission card `a`/`d` should answer from `cursor`, or `null`.
 *
 * The UI spec (§4.3): the cursor is on the permission request, **or on the tool call it gates**. A
 * pending card is anchored directly after its tool call (`timeline.ts`), so from a tool row the
 * card is the first permission item after it that names the same `toolUseId`. Any other row has
 * no card to answer, and `a`/`d` there do nothing, rather than reaching for the nearest card:
 * approving the wrong call is exactly what a keyboard shortcut must not make easy.
 */
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

/** One panel target of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md
 *  §2.2): a conversation row, a fenced code block inside one (which remembers its row, because
 *  landing on it moves the row cursor there), or an enabled control from `controlsOf`. The list is
 *  frozen at `hint_collect` and `shell` addresses its entries by index from then on. `rowIndex`
 *  is the row's index at that moment only; landing re-reads it from the element, because rows can
 *  be inserted above it while the labels are up. */
export type HintTarget =
  | { kind: "row"; el: HTMLElement; rowIndex: number }
  | { kind: "control"; el: HTMLElement }
  | { kind: "code"; el: HTMLElement; rowIndex: number }
  | { kind: "composer"; el: HTMLElement };

/** Marks the composer as a HINT target (spec §2.4, "面板输入框"). The composer is in no
 *  `data-nav-stop` -- `j`/`k` never walk into it, `i` and `Ctrl+l` are its routes -- and in BROWSE it
 *  is a plain `div`, which `CONTROL_SELECTOR` does not match, so without this marker HINT could never
 *  reach it (found in the whole-branch review). `Composer` sets it only while INPUT can be entered. */
export const HINT_COMPOSER_ATTR = "data-hint-composer";

/** The elements known to scroll their own content (index.css): the conversation's list, and the
 *  start screen's list of remembered sessions, capped at 40vh. Named here as well as read off the
 *  computed style below, because jsdom loads no stylesheet and the tests must see these two. */
const SCROLL_CONTAINERS = ".message-list, .session-choice";

function clips(el: HTMLElement): boolean {
  if (el.matches(SCROLL_CONTAINERS)) return true;
  const style = getComputedStyle(el);
  return [style.overflow, style.overflowX, style.overflowY].some((v) => v !== "" && v !== "visible");
}

function intersects(a: DOMRect, b: DOMRect): boolean {
  return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
}

/** Whether `el` shows on screen inside `root`: it has a non-zero box, and that box intersects the
 *  root and every ancestor between them that clips its overflow (spec §2.2, "visible inside its own
 *  scroll container"). Every ancestor, not only the nearest: a nested scroller's content is only on
 *  screen where each of them lets it through. */
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
