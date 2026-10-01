/** Where the conversation's view goes when `j`/`k` walk it, row by row, over rows of very different
 *  heights -- a two-line reply, a 120-line tool result, a 3000px essay, a one-line question.
 *
 *  The old rules leaned on the engine's `scrollIntoView({ block: "nearest" })` and on comparisons of
 *  fractional rects, and they failed at the seams between a long row and a short one: `k` into a row
 *  taller than the view moved a whole view in one press (the row's bottom was aligned to the view's
 *  bottom, so both the short row just left and the tall row's head left the screen); `j` into one
 *  did nothing at one zoom level, because the row's top sat 0.39px inside a fractional list rect, and
 *  a whole view at another. WebKitGTK 2.52.6's `nearest` also top-aligns any element taller than the
 *  view, whichever side it is on, which the specification does not.
 *
 *  So the target `scrollTop` is computed here, from whole-pixel geometry, and written directly. The
 *  decisions are pure functions of numbers (`decideLanding`, `decidePress`) so they can be tested
 *  exhaustively without a layout engine; the DOM half (`readRowGeometry`, `scrollListTo`) only reads
 *  rects and writes `scrollTop`.
 *
 *  Every coordinate is in the list's content space (a rect's top, less the list's top, plus its
 *  `scrollTop`), in CSS pixels, so a zoomed WebView and an unzoomed one give the same numbers. */

/** A press inside a row (or a tool result's box) scrolls this many lines of that row's own text. */
export const STEP_LINES = 3;

/** The lines of breathing room kept between the cursor row and the view's edge when it fits -- vim's
 *  `scrolloff`, so the row never sits flush on an edge and the neighbour it is about to move to is
 *  already half in sight. */
export const MARGIN_LINES = 2;

/** Where a tall row's head lands when `j` enters it, as a fraction of the view from the top: far enough
 *  down that the tail of the row just read stays in the top third, far enough up that the first lines of
 *  the new row are not at the very bottom. `k` mirrors it (the tail lands `1 - READING_LINE` down). */
export const READING_LINE = 1 / 3;

/** Sub-pixel slack for "this edge is inside": fractional scaling leaves rects a fraction of a pixel off,
 *  and a row whose last line is 0.3px past a margin line must not eat a keypress. */
export const SLACK_PX = 1;

/** How long a row-to-row or in-row scroll takes to ease out. Short enough that a reader never waits on
 *  it, long enough to read as one movement rather than a cut. */
export const SCROLL_ANIMATION_MS = 150;

export type Direction = 1 | -1;

/** The visible part of the list: where its top is in content space, how tall it is, and how far it can
 *  scroll at most (`Infinity` where the engine reports no scroll height, as jsdom does). */
export type View = { scrollTop: number; height: number; maxScrollTop: number };

/** A vertical extent in content space. */
export type Span = { top: number; bottom: number };

/** A tool result's own scrolling box: its extent in the list's content space, and its own scroll state. */
export type BoxState = Span & { scrollTop: number; clientHeight: number; scrollHeight: number };

const clampTo = (view: View, scrollTop: number) => Math.max(0, Math.min(view.maxScrollTop, scrollTop));

/** The margin to keep at each edge: `MARGIN_LINES` of the row's text, but never more than a quarter of the
 *  view. A panel squeezed to a few lines has no room for two lines at each edge -- the margin lines would
 *  cross, every row would count as "taller than the view", and a press would step the view past a row only to
 *  bring it back again. */
function marginFor(view: View, line: number): number {
  return Math.min(MARGIN_LINES * line, view.height / 4);
}

export type Landing = {
  /** The `scrollTop` to put the view at: the current one when nothing needs to move. */
  scrollTop: number;
  /** `stay`: the row is already where it belongs. `nearest`: a row that fits, brought in by the least
   *  scroll. `head`/`tail`: a row taller than the view, entered at the end nearest to where the reader
   *  came from. */
  how: "stay" | "nearest" | "head" | "tail";
};

/** Where the view should be once the cursor has landed on `row`.
 *
 *  `direction` is the key that brought the cursor there: `1` for `j`, `-1` for `k`, `0` for any other
 *  way (a search match, a jump, an arrival), which then judges from where the row is.
 *
 *  - A row already inside both margins moves nothing.
 *  - A row that fits (shorter than the view less both margins) is brought in by the least scroll that
 *    leaves it a margin on each side -- whichever way it lies from the view, whatever key came.
 *  - A taller row is entered at the end nearest the reader: going down its head goes to the reading
 *    line (a third of the way down), going up its tail goes to the mirrored line. It only ever scrolls
 *    in the direction of travel, so a head already above the reading line stays where it is.
 *
 *  The result is a whole pixel inside the list's own range. */
export function decideLanding(input: { direction: Direction | 0; view: View; row: Span; line: number }): Landing {
  const { direction, view, row, line } = input;
  const margin = marginFor(view, line);
  const top = view.scrollTop;
  const bottom = top + view.height;
  const stay: Landing = { scrollTop: top, how: "stay" };
  const to = (want: number, how: Landing["how"]): Landing => ({ scrollTop: clampTo(view, Math.round(want)), how });

  if (row.top >= top + margin - SLACK_PX && row.bottom <= bottom - margin + SLACK_PX) return stay;

  if (row.bottom - row.top <= view.height - 2 * margin + SLACK_PX) {
    return row.bottom > bottom - margin ? to(row.bottom - view.height + margin, "nearest") : to(row.top - margin, "nearest");
  }

  const toward = direction !== 0 ? direction : row.top >= bottom ? 1 : row.bottom <= top ? -1 : 0;
  if (toward === 0) return stay;
  return toward > 0
    ? to(Math.max(top, row.top - view.height * READING_LINE), "head")
    : to(Math.min(top, row.bottom - view.height * (1 - READING_LINE)), "tail");
}

/** The next `scrollTop` for a tool result's own box one press in `direction`, or `null` when it has
 *  nothing further to give that way. A step is `STEP_LINES` of the box's own lines; the last one is
 *  whatever remains. */
export function boxStepTarget(box: BoxState, direction: Direction, boxLine: number): number | null {
  const step = STEP_LINES * boxLine;
  if (direction > 0) {
    if (box.scrollTop + box.clientHeight >= box.scrollHeight - SLACK_PX) return null;
    return Math.round(Math.min(box.scrollTop + step, Math.max(0, box.scrollHeight - box.clientHeight)));
  }
  if (box.scrollTop <= 0) return null;
  return Math.round(Math.max(box.scrollTop - step, 0));
}

/** The next `scrollTop` for the list when the cursor row itself runs past a margin line in the
 *  direction of travel -- the row is taller than what is left of it on screen -- or `null` when it does
 *  not, or the list cannot move any further. The step is `STEP_LINES` of the row's text, never more than
 *  what is left to reveal, so the press that finishes leaves the row's end exactly on the margin line
 *  and the next press moves on. */
function rowStepTarget(input: { direction: Direction; view: View; row: Span; line: number }): number | null {
  const { direction, view, row, line } = input;
  const margin = marginFor(view, line);
  const step = STEP_LINES * line;
  const top = view.scrollTop;
  const bottom = top + view.height;
  let want: number;
  if (direction > 0) {
    if (row.bottom <= bottom - margin + SLACK_PX) return null;
    want = Math.min(top + step, row.bottom - view.height + margin);
  } else {
    if (row.top >= top + margin - SLACK_PX) return null;
    want = Math.max(top - step, row.top - margin);
  }
  const target = clampTo(view, Math.round(want));
  return Math.abs(target - top) >= SLACK_PX ? target : null;
}

export type Press =
  /** Scroll the cursor row's tool result box to `boxScrollTop`. */
  | { kind: "box"; boxScrollTop: number }
  /** Scroll the list to `scrollTop`: the cursor row is taller than what is left of it on screen. */
  | { kind: "row"; scrollTop: number }
  /** The cursor row was wholly out of view: bring it back, and let that be the press. */
  | { kind: "reveal"; scrollTop: number }
  /** Nothing to scroll in this row: the cursor moves to the neighbour. */
  | { kind: "move" };

/** What one `j`/`k` does while the cursor is on `row`, in this order: bring the row back if it is out of
 *  view; else step its tool result's box if that is on screen and can still move; else step the list
 *  through the row if it runs past a margin line; else move to the neighbouring row. */
export function decidePress(input: {
  direction: Direction;
  view: View;
  row: Span;
  line: number;
  box: BoxState | null;
  /** One line of the box's own text, which sets the box's step. */
  boxLine: number;
}): Press {
  const { direction, view, row, line, box, boxLine } = input;
  const top = view.scrollTop;
  const bottom = top + view.height;

  if (row.bottom <= top || row.top >= bottom) {
    // Judged from where the row lies, not from the key: a tall row above the view comes back at its tail
    // whichever way the press was going, since that is the end nearest the reader.
    const back = decideLanding({ direction: 0, view, row, line }).scrollTop;
    return Math.abs(back - top) >= SLACK_PX ? { kind: "reveal", scrollTop: back } : { kind: "move" };
  }
  if (box !== null && box.bottom > top && box.top < bottom) {
    const next = boxStepTarget(box, direction, boxLine);
    if (next !== null) return { kind: "box", boxScrollTop: next };
  }
  const target = rowStepTarget({ direction, view, row, line });
  return target === null ? { kind: "move" } : { kind: "row", scrollTop: target };
}

// ---------------------------------------------------------------------------------------------
// The DOM half: reading a list's geometry, and writing its `scrollTop`.
// ---------------------------------------------------------------------------------------------

/** The computed line height of `el`'s own text, in pixels -- ONE line. `line-height: normal` (and
 *  jsdom, which computes nothing) has no pixel value, so it falls back to 1.2 x the font size, the usual
 *  `normal`; with no font size either, to a 16px font. */
export function computedLineHeight(el: HTMLElement): number {
  const style = getComputedStyle(el);
  let line = parseFloat(style.lineHeight);
  if (!Number.isFinite(line) || line <= 0) {
    const font = parseFloat(style.fontSize);
    line = (Number.isFinite(font) && font > 0 ? font : 16) * 1.2;
  }
  return line;
}

/** The element whose text the reader actually reads on a row: `.row-body` (`Row.tsx`'s own text cell),
 *  the only part of a row `index.css` gives a font size and line height of its own -- `.row` itself sets
 *  neither, so it reads whatever is ambient there (~16.8px against `.row-body`'s ~24.75px). Falls back to
 *  `row` for anything with no `.row-body` child. */
export function rowTextElement(row: HTMLElement): HTMLElement {
  return row.querySelector<HTMLElement>(".row-body") ?? row;
}

export type RowGeometry = { view: View; row: Span; line: number };

/** The list's view and `row`'s extent, in whole pixels, or `null` when there is no layout to judge (the
 *  list has no height: jsdom, or a panel that is not laid out yet).
 *
 *  The view is `clientHeight`, the integer the engine clamps `scrollTop` against; its rect is a
 *  fraction taller (900.391px against 900) and measuring against that is what made one press at one
 *  zoom level dead and another a whole view. Where an environment reports no `clientHeight`, the rect's
 *  rounded height stands in. */
export function readRowGeometry(list: HTMLElement, row: HTMLElement): RowGeometry | null {
  const l = list.getBoundingClientRect();
  const rectHeight = l.bottom - l.top;
  if (!(rectHeight > 0)) return null;
  const height = list.clientHeight > 0 ? list.clientHeight : Math.round(rectHeight);
  const scrollTop = list.scrollTop;
  const r = row.getBoundingClientRect();
  return {
    view: {
      scrollTop,
      height,
      maxScrollTop: list.scrollHeight > 0 ? Math.max(0, list.scrollHeight - height) : Number.POSITIVE_INFINITY,
    },
    row: { top: Math.round(r.top - l.top + scrollTop), bottom: Math.round(r.bottom - l.top + scrollTop) },
    line: computedLineHeight(rowTextElement(row)),
  };
}

/** `box`'s extent and scroll state, in the same space as `readRowGeometry`'s `row`. */
export function readBoxState(list: HTMLElement, box: HTMLElement): BoxState {
  const l = list.getBoundingClientRect();
  const b = box.getBoundingClientRect();
  return {
    top: Math.round(b.top - l.top + list.scrollTop),
    bottom: Math.round(b.bottom - l.top + list.scrollTop),
    scrollTop: box.scrollTop,
    clientHeight: box.clientHeight,
    scrollHeight: box.scrollHeight,
  };
}

type Running = { frame: number; target: number; lastWritten: number };
const running = new WeakMap<HTMLElement, Running>();

/** Whether to skip the ease-out: the system asks for reduced motion (WebKitGTK follows GTK's
 *  `gtk-enable-animations` here), or there is no way to ask -- an environment without `matchMedia` gets
 *  the plain, immediate scroll rather than a guess. */
function reducedMotion(): boolean {
  return typeof window === "undefined" || typeof window.matchMedia !== "function"
    ? true
    : window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** Whether a scroll this module started on `list` is still easing toward its target, under its own power:
 *  the list is where the ease last put it. The list's own scroll listener holds off re-homing the cursor
 *  while this is true -- the intermediate frames can have the cursor's row, still on its way in, wholly
 *  outside the view for a moment -- and a scroll event that finds the list anywhere else (a wheel, say)
 *  is somebody else's, and is judged as any other. */
export function isListScrollAnimating(list: HTMLElement): boolean {
  const state = running.get(list);
  return state !== undefined && Math.abs(list.scrollTop - state.lastWritten) <= SLACK_PX;
}

/** Puts a running scroll's list at its target now and stops it, so the next decision starts from where
 *  that scroll was going, never from a frame in between. A list somebody else has moved since (a wheel,
 *  a streamed row's snap to the end) is left where it is. */
export function settleListScroll(list: HTMLElement): void {
  const state = running.get(list);
  if (state === undefined) return;
  cancelAnimationFrame(state.frame);
  running.delete(list);
  if (Math.abs(list.scrollTop - state.lastWritten) <= SLACK_PX) list.scrollTop = state.target;
}

/** Drops a running scroll without moving the list. For a restored view (a tab's, an arrival's), which
 *  replaces whatever was easing: the "has somebody else written the list?" check inside the ease cannot see
 *  a restore that writes the very position the ease began at. */
export function cancelListScroll(list: HTMLElement): void {
  const state = running.get(list);
  if (state === undefined) return;
  cancelAnimationFrame(state.frame);
  running.delete(list);
}

/** A move shorter than this is made at once: too small to see as a glide, and the first frame's share of
 *  anything larger is then at least two pixels, which is more than the "at the very bottom" tolerance a
 *  queued scroll event is judged by. */
const MIN_EASED_PX = 8;

/** How far into the ease the first write is: one frame's worth, which is also where the first animation
 *  frame would put it. */
const FIRST_FRAME_MS = 16;

const easeOut = (t: number) => 1 - (1 - t) ** 3;

/** Moves `list` to `target` (a whole pixel inside its range). With `animate` the move eases out over
 *  `SCROLL_ANIMATION_MS`; without it -- a held key's repeats, a count, a system with animations off, a
 *  move too small to see -- it is immediate. An animation yields the moment anything else writes the list's
 *  `scrollTop` (the follow-the-newest snap, a wheel, a restored view), and never fights it.
 *
 *  The first frame's share of an eased move is written in the same task as the call, not left to the first
 *  animation frame: a scroll event queued by an earlier write is delivered before that frame, and one that
 *  finds the view still at the bottom re-arms following -- undoing the `k` that began this very move. */
export function scrollListTo(list: HTMLElement, target: number, animate: boolean): void {
  cancelListScroll(list);
  const from = list.scrollTop;
  if (Math.abs(target - from) < SLACK_PX / 2) return;
  if (!animate || reducedMotion() || Math.abs(target - from) < MIN_EASED_PX) {
    list.scrollTop = target;
    return;
  }
  const startedAt = performance.now() - FIRST_FRAME_MS;
  list.scrollTop = Math.round(from + (target - from) * easeOut(FIRST_FRAME_MS / SCROLL_ANIMATION_MS));
  const state: Running = { frame: 0, target, lastWritten: list.scrollTop };
  const step = (now: number) => {
    if (Math.abs(list.scrollTop - state.lastWritten) > SLACK_PX) {
      running.delete(list);
      return;
    }
    const t = Math.min(1, Math.max(0, now - startedAt) / SCROLL_ANIMATION_MS);
    list.scrollTop = t >= 1 ? target : Math.round(from + (target - from) * easeOut(t));
    state.lastWritten = list.scrollTop;
    if (t >= 1) running.delete(list);
    else state.frame = requestAnimationFrame(step);
  };
  running.set(list, state);
  state.frame = requestAnimationFrame(step);
}
