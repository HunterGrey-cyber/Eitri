/** A held `j`/`k`: the conversation glides at a steady speed, the way a browser scrolls a page while an
 *  arrow key is held, and the cursor follows the row under its line, instead of a train of jumps at the
 *  key-repeat rate (one row, or three lines of a tall row, per repeat event, of whatever size that was).
 *
 *  The first press is still `./jkScroll`'s eased step. From the first repeat the view is driven by
 *  `requestAnimationFrame`: repeats only keep the glide alive, and the key's release (or a repeat that does
 *  not come, or the window losing focus) ends it with a short stop that leaves the cursor row where a press
 *  would have put it (`decideLanding`).
 *
 *  `HoldMotion` is the motion as numbers, with no DOM, so it can be tested frame by frame; `startHold` and
 *  its companions drive it on a real list. Every coordinate is in the list's content space, as in
 *  `./jkScroll`. */
import {
  cancelListScroll,
  computedLineHeight,
  decideLanding,
  READING_LINE,
  reducedMotion,
  registerHold,
  rowTextElement,
  scrollListTo,
  SLACK_PX,
  STEP_LINES,
} from "./jkScroll";
import type { Direction, Span, View } from "./jkScroll";

/** How far the view moves per key-repeat interval while held: what one repeat used to scroll inside a tall
 *  row. So the glide covers what the repeats covered, at the same rate, without the jumps; and the speed
 *  is set by the text's line height alone, never by how tall the rows passing by are. */
export const HOLD_LINES_PER_REPEAT = STEP_LINES;

/** The repeat interval assumed until two repeats have shown the real one: 30 per second, a common
 *  default. The page cannot ask the system for its setting. */
export const DEFAULT_REPEAT_MS = 1000 / 30;

/** The bounds a measured repeat interval is held to: a jittery first pair of repeats must not make the
 *  glide absurdly fast or slow. */
export const MIN_REPEAT_MS = 20;
export const MAX_REPEAT_MS = 100;

/** How long the glide takes to reach its speed from rest, as a browser's held arrow key does: long enough
 *  not to read as a jolt, short enough that the hold is at speed before the reader notices it gathering. */
export const HOLD_RAMP_MS = 120;

/** How quickly the view closes the distance when the cursor line is not on its reading line (the cursor
 *  started near an edge): the view moves faster, or slower, by this distance over this time, so the line
 *  drifts to its place instead of the view lurching to put it there. */
export const HOLD_CATCH_UP_MS = 400;

/** A missing repeat for this many repeat intervals ends the hold as a release would: the release itself
 *  was lost (a keyup another window took), or the key repeat stopped. */
export const HOLD_STALL_REPEATS = 2;

/** The bounds on that wait: a fast repeat rate must not end a hold on one late event, a slow one must not
 *  keep gliding long after the key was let go. */
export const MIN_STALL_MS = 80;
export const MAX_STALL_MS = 200;

/** How long the glide's stop takes, unhurried: the view coasts on for a third of this at the speed it
 *  had, which is how far an ease-out from that speed travels. */
export const HOLD_SETTLE_MS = 120;

/** The most one frame moves the glide by, in ms of its time: two frames at 60 per second. */
export const MAX_FRAME_MS = 34;

/** The view's speed for a line height (px) and a repeat interval (ms), in px/ms. */
export function holdSpeed(line: number, repeatMs: number): number {
  const interval = Math.min(MAX_REPEAT_MS, Math.max(MIN_REPEAT_MS, repeatMs));
  return (HOLD_LINES_PER_REPEAT * line) / interval;
}

/** How long a hold waits for a repeat before it ends, for a repeat interval (ms). */
export function stallAfter(repeatMs: number): number {
  return Math.min(MAX_STALL_MS, Math.max(MIN_STALL_MS, HOLD_STALL_REPEATS * repeatMs));
}

/** The share of full speed `elapsed` ms into the hold. */
function ramp(elapsed: number): number {
  return Math.min(1, Math.max(0, elapsed) / HOLD_RAMP_MS);
}

/** The distance, in units of full speed (ms at speed), covered in the first `elapsed` ms of a hold whose
 *  speed rises evenly to full over `HOLD_RAMP_MS`. Exact, so the distance does not depend on frame times. */
function rampedTime(elapsed: number): number {
  const e = Math.max(0, elapsed);
  return e < HOLD_RAMP_MS ? (e * e) / (2 * HOLD_RAMP_MS) : e - HOLD_RAMP_MS / 2;
}

/** The row the cursor line `y` is on. In the gap between two rows it is the row the line has just left,
 *  so the cursor never runs ahead of the line: the one above going down, the one below going up. */
export function rowAtLine(rows: readonly Span[], y: number, direction: Direction): number {
  let lo = 0;
  let hi = rows.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (rows[mid].top <= y) {
      found = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  if (found === -1) return 0;
  if (direction < 0 && y >= rows[found].bottom && found + 1 < rows.length) return found + 1;
  return found;
}

/** The view's margin for a line, as `./jkScroll` keeps it. */
function marginFor(height: number, line: number): number {
  return Math.min(2 * line, height / 4);
}

export type HoldFrame = {
  /** Where the list goes this frame, a whole pixel. */
  scrollTop: number;
  /** The row under the cursor line. */
  cursor: number;
  /** The cursor line reached the first row's top (`k`) or the last row's end (`j`): nothing is left to
   *  glide over. */
  atEnd: boolean;
};

export type HoldSettle = {
  /** The row the cursor ends on: the one under the line when the key was let go. */
  cursor: number;
  /** Where the view comes to rest. */
  target: number;
  /** The view's speed when the key was let go, px/ms, signed, for a stop with no jolt. */
  velocity: number;
};

/** The glide as numbers. The cursor line (`y`) moves at the hold's speed from the first repeat; the view
 *  (`s`, its `scrollTop`) moves with it so that the line sits on the reading line -- a third of the way
 *  down going down, two thirds going up, as `decideLanding` places a tall row -- drifting there if the line
 *  started elsewhere. At the list's ends the view stops and the line goes on to the first or last row. */
export class HoldMotion {
  readonly direction: Direction;
  private rows: readonly Span[];
  private height: number;
  private maxScrollTop: number;
  private readonly line: number;
  private speed: number;
  private y: number;
  private s: number;
  private last: number;
  /** The hold's own time: the frame times since it started, each held to `MAX_FRAME_MS`. */
  private elapsed = 0;
  private viewSpeed = 0;
  private current: number;

  constructor(input: { direction: Direction; rows: readonly Span[]; cursor: number; view: View; line: number; speed: number; now: number }) {
    this.direction = input.direction;
    this.rows = input.rows;
    this.height = input.view.height;
    this.maxScrollTop = input.view.maxScrollTop;
    this.line = input.line;
    this.speed = input.speed;
    this.s = input.view.scrollTop;
    this.last = input.now;
    this.current = input.cursor;
    const row = input.rows[input.cursor];
    // The line starts inside the cursor's row, as near the reading line as the row reaches: on it when the
    // row crosses it, else at the row's end nearest to it. It never starts in another row.
    const reading = this.s + this.anchor();
    this.y = row === undefined ? reading : Math.min(row.bottom - 1, Math.max(row.top, reading));
  }

  /** The reading line's place in the view, from its top. */
  private anchor(): number {
    return this.direction > 0 ? this.height * READING_LINE : this.height * (1 - READING_LINE);
  }

  private yMin(): number {
    return this.rows.length === 0 ? this.y : this.rows[0].top;
  }

  private yMax(): number {
    return this.rows.length === 0 ? this.y : this.rows[this.rows.length - 1].bottom - 1;
  }

  /** Whether the line has anywhere to go: the hold is worth starting. */
  canMove(): boolean {
    if (this.rows.length === 0 || this.rows[this.current] === undefined) return false;
    // On the last row (first, going up) with the view at the end there is nothing to glide to: the
    // repeat is a press, which says so (`j` on the last row) or steps off the rows.
    const lastRow = this.direction > 0 ? this.current === this.rows.length - 1 : this.current === 0;
    const viewAtEnd = this.direction > 0 ? this.s >= this.maxScrollTop - SLACK_PX : this.s <= SLACK_PX;
    if (lastRow && viewAtEnd) return false;
    return this.direction > 0 ? this.y < this.yMax() : this.y > this.yMin();
  }

  /** The speed, once the repeats have shown their real interval. */
  setSpeed(speed: number): void {
    this.speed = speed;
  }

  /** The list was moved by something else (a follow-the-newest snap): the glide goes on from there. */
  adopt(scrollTop: number): void {
    this.s = scrollTop;
  }

  /** The rows or the view changed: the line keeps its place inside the cursor's row, which is now row
   *  `cursor` (rows above it may have come, gone or folded), and the view is where the list now is. */
  rebase(rows: readonly Span[], view: View, cursor: number = this.current): void {
    const before = this.rows[this.current];
    const after = rows[cursor];
    if (before !== undefined && after !== undefined) {
      this.y = Math.min(after.bottom - 1, Math.max(after.top, after.top + (this.y - before.top)));
    }
    this.rows = rows;
    this.height = view.height;
    this.maxScrollTop = view.maxScrollTop;
    this.s = view.scrollTop;
    this.current = rowAtLine(rows, this.y, this.direction);
  }

  /** The motion at `now` (ms, the frame's time). */
  advance(now: number): HoldFrame {
    const d = this.direction;
    // A frame that came late moves the glide by at most `MAX_FRAME_MS`: the time lost in a stall is not
    // made up in one leap, which would read as a jump right after the pause.
    const dt = Math.min(MAX_FRAME_MS, Math.max(0, now - this.last));
    const from = this.elapsed;
    this.elapsed += dt;
    const lineTravel = this.speed * (rampedTime(this.elapsed) - rampedTime(from));
    const yFrom = this.y;
    const yTo = this.y + d * lineTravel;
    const yEnd = d > 0 ? this.yMax() : this.yMin();
    const atEnd = d > 0 ? yTo >= yEnd : yTo <= yEnd;
    this.y = atEnd ? yEnd : yTo;

    // The view: the line's speed, plus what closes the gap to the reading line over `HOLD_CATCH_UP_MS`,
    // never backwards, gathering speed with the line. Judged from where the line was at the frame's
    // start, as the view is, so a line on its reading line keeps it.
    const ahead = d * (yFrom - this.s - this.anchor());
    const midRamp = ramp((from + this.elapsed) / 2);
    this.viewSpeed = midRamp * Math.max(0, this.speed + ahead / HOLD_CATCH_UP_MS);
    let s = this.s + d * this.viewSpeed * dt;
    // A view too short for the line to drift in keeps the line inside its margins outright.
    const margin = marginFor(this.height, this.line);
    if (d > 0 && this.y > s + this.height - margin) s = this.y - (this.height - margin);
    if (d < 0 && this.y < s + margin) s = this.y - margin;
    const clamped = Math.max(0, Math.min(this.maxScrollTop, s));
    if (clamped !== s) this.viewSpeed = 0;
    this.s = clamped;
    this.last = now;
    this.current = rowAtLine(this.rows, this.y, d);
    return { scrollTop: Math.round(this.s), cursor: this.current, atEnd };
  }

  /** The key was let go: the cursor stays on the row under the line, the view coasts on a little at the
   *  speed it had, then comes to rest where `decideLanding` puts that row -- moving on in the direction it
   *  was going if the row needs it, never back. The coast never carries the line past the trailing margin,
   *  so the cursor's row stays in view. */
  release(): HoldSettle {
    const d = this.direction;
    const row = this.rows[this.current];
    const margin = marginFor(this.height, this.line);
    const room = d > 0 ? this.y - this.s - margin : this.s + this.height - this.y - margin;
    const coast = Math.min((this.viewSpeed * HOLD_SETTLE_MS) / 3, Math.max(0, room));
    const coasted = Math.max(0, Math.min(this.maxScrollTop, this.s + d * coast));
    let target = coasted;
    if (row !== undefined) {
      const landing = decideLanding({
        direction: d,
        view: { scrollTop: coasted, height: this.height, maxScrollTop: this.maxScrollTop },
        row,
        line: this.line,
      });
      if (landing.how !== "stay" && d * (landing.scrollTop - coasted) > 0) target = landing.scrollTop;
    }
    return { cursor: this.current, target: Math.round(target), velocity: d * this.viewSpeed };
  }

  /** For tests and the frame-timing hook: the cursor line and the view, unrounded. */
  get state(): { y: number; scrollTop: number; viewSpeed: number } {
    return { y: this.y, scrollTop: this.s, viewSpeed: this.viewSpeed };
  }
}

// ---------------------------------------------------------------------------------------------
// The DOM half: one glide per list, driven by animation frames.
// ---------------------------------------------------------------------------------------------

export type HoldOptions = {
  list: HTMLElement;
  /** The conversation rows (`./nav`'s `conversationRows`), read again each frame: index `i` is timeline
   *  index `i`, and a different array means the rows changed. */
  rows: () => readonly HTMLElement[];
  cursor: number;
  direction: Direction;
  /** The held key, as its keydown's `code` (or `key` where there is no code): its keyup ends the hold. */
  code: string;
  /** When the repeat that started the hold came, on `performance.now()`'s clock. */
  now: number;
  /** The cursor moved to row `index`, which is under the cursor line and so on screen. */
  onCursor: (index: number) => void;
  /** The hold ended by a release, a missing repeat, focus lost or the end of the list, at `at` (on
   *  `performance.now()`'s clock). Not called for `cancelHold`. */
  onRelease: (at: number) => void;
};

type Hold = {
  options: HoldOptions;
  motion: HoldMotion;
  line: number;
  rowEls: readonly HTMLElement[];
  cursor: number;
  frame: number;
  /** The previous frame's time. */
  lastFrameAt: number;
  lastWritten: number;
  lastRepeat: number;
  repeatMs: number;
  repeats: number;
  /** The first intervals between repeats, until there are `MEASURED_REPEATS` of them. */
  intervals: number[];
  scrollHeight: number;
  clientHeight: number;
  end: (how: "release" | "cancel", at: number) => void;
};

const holds = new WeakMap<HTMLElement, Hold>();

/** A per-frame report for the GUI pass's frame-timing measurement: set on `window.__eitriHoldTrace` by the
 *  harness only; nothing in the panel sets it. */
type HoldTrace = (frame: { t: number; scrollTop: number; cursor: number; y: number; speed: number }) => void;

function trace(): HoldTrace | null {
  const hook = (globalThis as { __eitriHoldTrace?: unknown }).__eitriHoldTrace;
  return typeof hook === "function" ? (hook as HoldTrace) : null;
}

/** The list's view and every row's extent, in content space, whole pixels; `null` with no layout. */
function measure(list: HTMLElement, rows: readonly HTMLElement[]): { view: View; spans: Span[] } | null {
  const l = list.getBoundingClientRect();
  const rectHeight = l.bottom - l.top;
  if (!(rectHeight > 0)) return null;
  const height = list.clientHeight > 0 ? list.clientHeight : Math.round(rectHeight);
  const scrollTop = list.scrollTop;
  const spans = rows.map((row) => {
    const r = row.getBoundingClientRect();
    return { top: Math.round(r.top - l.top + scrollTop), bottom: Math.round(r.bottom - l.top + scrollTop) };
  });
  return {
    view: {
      scrollTop,
      height,
      maxScrollTop: list.scrollHeight > 0 ? Math.max(0, list.scrollHeight - height) : Number.POSITIVE_INFINITY,
    },
    spans,
  };
}

/** Starts a glide on `options.list` from the cursor's row, or returns `false` -- nothing to glide over,
 *  no layout to judge, or the system asks for reduced motion -- and the repeat is handled as a press. */
export function startHold(options: HoldOptions): boolean {
  const { list, direction } = options;
  if (holds.has(list) || reducedMotion()) return false;
  const rowEls = options.rows();
  const row = rowEls[options.cursor];
  if (row === undefined) return false;
  const measured = measure(list, rowEls);
  if (measured === null) return false;
  const line = computedLineHeight(rowTextElement(row));
  const motion = new HoldMotion({
    direction,
    rows: measured.spans,
    cursor: options.cursor,
    view: measured.view,
    line,
    speed: holdSpeed(line, DEFAULT_REPEAT_MS),
    now: options.now,
  });
  if (!motion.canMove()) return false;
  // An eased step still running (the first press's) is dropped where it is, not finished: finishing it
  // would be a jump right as the glide begins.
  cancelListScroll(list);

  const onKeyUp = (event: KeyboardEvent) => {
    if ((event.code || event.key) === options.code) hold.end("release", performance.now());
  };
  const onBlur = () => hold.end("release", performance.now());
  const onVisibility = () => {
    if (document.visibilityState !== "visible") hold.end("release", performance.now());
  };
  const detach = () => {
    window.removeEventListener("keyup", onKeyUp, true);
    window.removeEventListener("blur", onBlur);
    document.removeEventListener("visibilitychange", onVisibility);
  };

  const hold: Hold = {
    options,
    motion,
    line,
    rowEls,
    cursor: options.cursor,
    frame: 0,
    lastFrameAt: Number.POSITIVE_INFINITY,
    lastWritten: list.scrollTop,
    lastRepeat: options.now,
    repeatMs: DEFAULT_REPEAT_MS,
    repeats: 0,
    intervals: [],
    scrollHeight: list.scrollHeight,
    clientHeight: list.clientHeight,
    end: (how, at) => {
      if (holds.get(list) !== hold) return;
      holds.delete(list);
      registerHold(list, null);
      cancelAnimationFrame(hold.frame);
      detach();
      if (how === "cancel") return;
      const settle = motion.release();
      if (settle.cursor !== hold.cursor) {
        hold.cursor = settle.cursor;
        options.onCursor(settle.cursor);
      }
      scrollListTo(list, settle.target, true, settle.velocity);
      options.onRelease(at);
    },
  };

  const frame = (t: number) => {
    hold.frame = 0;
    if (holds.get(list) !== hold) return;
    if (Math.abs(list.scrollTop - hold.lastWritten) > SLACK_PX) motion.adopt(list.scrollTop);
    const rowsNow = options.rows();
    if (rowsNow !== hold.rowEls || list.scrollHeight !== hold.scrollHeight || list.clientHeight !== hold.clientHeight) {
      // The cursor's row is followed as an element, not an index: rows above it can fold or go away.
      const at = rowsNow.indexOf(hold.rowEls[hold.cursor]);
      const again = measure(list, rowsNow);
      if (again === null || at === -1) {
        hold.end("cancel", t);
        return;
      }
      hold.rowEls = rowsNow;
      hold.cursor = at;
      hold.scrollHeight = list.scrollHeight;
      hold.clientHeight = list.clientHeight;
      motion.rebase(again.spans, again.view, at);
    }
    // A missing repeat is judged only on a frame that came on time: after a frame the page was too busy
    // to draw, the repeats queued meanwhile have not been delivered yet, and they would end the hold for
    // nothing (the next one then starting it again from rest).
    const late = t - hold.lastFrameAt > 2 * MAX_FRAME_MS;
    hold.lastFrameAt = t;
    // Until the repeats have shown their interval, the longest wait: a slow repeat rate must not end the
    // hold before its second repeat comes.
    const stall = hold.intervals.length < MEASURED_REPEATS ? MAX_STALL_MS : stallAfter(hold.repeatMs);
    if (!late && performance.now() - hold.lastRepeat > stall) {
      hold.end("release", performance.now());
      return;
    }
    const next = motion.advance(t);
    list.scrollTop = next.scrollTop;
    hold.lastWritten = list.scrollTop;
    if (next.cursor !== hold.cursor) {
      hold.cursor = next.cursor;
      options.onCursor(next.cursor);
    }
    trace()?.({ t, scrollTop: list.scrollTop, cursor: next.cursor, y: motion.state.y, speed: motion.state.viewSpeed });
    if (next.atEnd) {
      hold.end("release", performance.now());
      return;
    }
    hold.frame = requestAnimationFrame(frame);
  };

  holds.set(list, hold);
  registerHold(list, { lastWritten: () => hold.lastWritten, cancel: () => hold.end("cancel", performance.now()) });
  window.addEventListener("keyup", onKeyUp, true);
  window.addEventListener("blur", onBlur);
  document.addEventListener("visibilitychange", onVisibility);
  hold.frame = requestAnimationFrame(frame);
  return true;
}

/** How many intervals between repeats set the hold's speed: their median, so one repeat delivered late
 *  by a busy page does not set it. Three come within about 100ms, while the glide is still gathering. */
export const MEASURED_REPEATS = 3;

/** A repeat of the held key at `now`: keeps the glide on `list` going and returns `true`, or `false` when
 *  no glide of that key is running there. The first `MEASURED_REPEATS` intervals between repeats show the
 *  repeat interval, which sets the speed for the rest of the hold. */
export function keepHoldAlive(list: HTMLElement, code: string, now: number): boolean {
  const hold = holds.get(list);
  if (hold === undefined || hold.options.code !== code) return false;
  if (hold.intervals.length < MEASURED_REPEATS) {
    hold.intervals.push(now - hold.lastRepeat);
    if (hold.intervals.length === MEASURED_REPEATS) {
      const sorted = [...hold.intervals].sort((a, b) => a - b);
      hold.repeatMs = Math.min(MAX_REPEAT_MS, Math.max(MIN_REPEAT_MS, sorted[Math.floor(sorted.length / 2)]));
      hold.motion.setSpeed(holdSpeed(hold.line, hold.repeatMs));
    }
  }
  hold.repeats += 1;
  hold.lastRepeat = now;
  return true;
}

/** Whether a glide is running on `list`. */
export function isHolding(list: HTMLElement): boolean {
  return holds.has(list);
}

/** Ends the glide on `list` as a release does (another key came): the stop, then `onRelease`. */
export function releaseHold(list: HTMLElement, at: number): void {
  holds.get(list)?.end("release", at);
}

/** Ends the glide on `list` where it is, with no stop and no `onRelease`: something else now decides
 *  where the view goes (an arrival, a tab switch, a restored view). */
export function cancelHold(list: HTMLElement): void {
  holds.get(list)?.end("cancel", performance.now());
}
