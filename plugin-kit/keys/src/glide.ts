import { onRemove } from "./bridge";
/** Frame-driven scrolling for the generic scroll actions.
 *
 *  A key press eases one move, and presses that come while a move runs add to where it is heading, so N presses
 *  move exactly N steps. A held `j`/`k` does not start a move per key repeat: from the first repeat the view glides
 *  at one step per repeat interval, advanced on every animation frame, until the key is let go (or a repeat does not
 *  come), and then eases to rest from the speed it had. Every write is an instant one, so neither the engine's own
 *  smooth scrolling nor a page's `scroll-behavior` can restart a motion underneath it.
 *
 *  The numbers below match the agent panel's held-key glide so that the two places feel alike. Only the numbers are
 *  shared: the panel's motion follows a cursor over conversation rows and lives in a separate bundle, while a plugin
 *  page has no rows, only a scroll position. */
import { currentSettings } from "./settings";

/** How long a press from rest takes to ease out. */
export const PRESS_MS = 150;
/** The shortest and longest a move that starts at speed takes: short enough that the stop reads as the end of the
 *  motion, not a second movement; long enough that a far target does not lurch. */
export const RETARGET_MIN_MS = 60;
export const RETARGET_MAX_MS = 250;
/** The repeat interval assumed until three intervals have shown the real one: 30 per second, a common default. A
 *  page cannot ask the system for its setting. */
export const DEFAULT_REPEAT_MS = 1000 / 30;
/** The bounds a measured repeat interval is held to, so a jittery start cannot make the glide absurdly fast or
 *  slow. */
export const MIN_REPEAT_MS = 20;
export const MAX_REPEAT_MS = 100;
/** How long a hold takes to reach its speed: long enough not to read as a jolt, short enough to be at speed before
 *  the reader notices it gathering. */
export const RAMP_MS = 120;
/** The most one frame advances a hold by, in ms of its time: a late frame is not made up with a leap. */
export const MAX_FRAME_MS = 34;
/** How long the stop after a hold takes; the view coasts a third of this at the speed it had. */
export const SETTLE_MS = 120;

const MIN_STALL_MS = 80;
const MAX_STALL_MS = 200;
const STALL_REPEATS = 2;
/** The number of intervals between repeats whose median sets the hold's speed: one late repeat cannot skew it. */
const MEASURED_INTERVALS = 3;
const DEFAULT_STEP = 60;

/** How long a hold waits for a repeat before it ends as a release would (a keyup another window took, or the key
 *  repeat stopped), for a repeat interval in ms. */
export function stallAfter(repeatMs: number): number {
  return clamp(STALL_REPEATS * repeatMs, MIN_STALL_MS, MAX_STALL_MS);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** One eased move: from `from` to `to`, leaving at `v0` px/ms (0: a plain ease-out) and arriving at rest. */
type Curve = { from: number; to: number; v0: number; duration: number; start: number };

type Hold = {
  direction: 1 | -1;
  /** The hold's own time: frame gaps, each held to `MAX_FRAME_MS`. */
  elapsed: number;
  last: number;
  lastRepeat: number;
  intervals: number[];
  repeatMs: number;
  /** The speed is `rampFrom` at `rampAt` (hold time) and rises or falls evenly to `target` over `RAMP_MS`. */
  rampFrom: number;
  rampAt: number;
  target: number;
};

export type GlidePhase = "rest" | "press" | "hold" | "settle";

/** The motion as numbers, with no DOM and no clock of its own: every call says what time it is. Positions are in
 *  the scroller's content space, `0..max`. */
export class GlideMotion {
  /** The distance a hold covers per repeat interval: the scroll step. */
  step: number;
  private pos: number;
  private max: number;
  private curve: Curve | null = null;
  private settling = false;
  private held: Hold | null = null;
  /** Signed px/ms as of the last frame or call. */
  private velocity = 0;

  constructor(position: number, max: number, step = DEFAULT_STEP) {
    this.max = Math.max(0, max);
    this.pos = clamp(position, 0, this.max);
    this.step = step;
  }

  /** Where the running press or stop is heading; `null` at rest and during a hold. */
  get destination(): number | null {
    return this.curve ? this.curve.to : null;
  }

  get phase(): GlidePhase {
    if (this.held) return "hold";
    if (this.curve) return this.settling ? "settle" : "press";
    return "rest";
  }

  get position(): number {
    return this.pos;
  }

  /** Signed px/ms. */
  get speed(): number {
    return this.velocity;
  }

  get moving(): boolean {
    return this.held !== null || this.curve !== null;
  }

  setMax(max: number): void {
    this.max = Math.max(0, max);
    this.pos = clamp(this.pos, 0, this.max);
  }

  /** An eased move to `destination`. A press while a move runs the same way keeps its speed; a press against it
   *  starts afresh from where the view is. */
  press(destination: number, now: number): void {
    const from = this.positionAt(now);
    const v = this.velocityAt(now);
    this.held = null;
    this.settling = false;
    this.pos = from;
    this.startCurve(from, clamp(destination, 0, this.max), v, now);
  }

  /** A press by `delta`: added to where the running move is heading, so presses add up exactly. A press against
   *  the motion is not added to it: the view would first carry on the old way (a hold's coast is longer than a
   *  step), so it moves one step from where it is now. */
  pressBy(delta: number, now: number): void {
    const v = this.velocityAt(now);
    const against = v !== 0 && Math.sign(v) !== Math.sign(delta);
    const base = !against && this.curve ? this.curve.to : this.positionAt(now);
    this.press(base + delta, now);
  }

  /** A key repeat in `direction`: starts the hold, or keeps it alive and learns the repeat interval. */
  hold(direction: 1 | -1, now: number): void {
    const h = this.held;
    if (h && h.direction === direction) {
      if (h.intervals.length < MEASURED_INTERVALS) {
        h.intervals.push(now - h.lastRepeat);
        if (h.intervals.length === MEASURED_INTERVALS) {
          const sorted = [...h.intervals].sort((a, b) => a - b);
          h.repeatMs = clamp(sorted[1], MIN_REPEAT_MS, MAX_REPEAT_MS);
          // The new speed is reached over a ramp from the current one, so learning it is not a jolt.
          h.rampFrom = this.speedAt(h, h.elapsed);
          h.rampAt = h.elapsed;
          h.target = this.step / h.repeatMs;
        }
      }
      h.lastRepeat = now;
      return;
    }
    const from = this.positionAt(now);
    if (direction > 0 ? from >= this.max : from <= 0) {
      this.pos = from;
      this.stop();
      return;
    }
    // A press still easing hands its speed to the hold, which ramps from there: no dip as the glide takes over.
    const carried = Math.max(0, direction * this.velocityAt(now));
    this.pos = from;
    this.curve = null;
    this.settling = false;
    this.held = {
      direction,
      elapsed: 0,
      last: now,
      lastRepeat: now,
      intervals: [],
      repeatMs: DEFAULT_REPEAT_MS,
      rampFrom: carried,
      rampAt: 0,
      target: this.step / DEFAULT_REPEAT_MS,
    };
    this.velocity = direction * carried;
  }

  /** The key was let go: coast on a third of `SETTLE_MS` at the speed the view had, and come to rest. */
  release(now: number): void {
    const h = this.held;
    if (!h) return;
    const speed = Math.abs(this.velocity);
    this.held = null;
    const to = clamp(this.pos + (h.direction * speed * SETTLE_MS) / 3, 0, this.max);
    this.settling = true;
    this.startCurve(this.pos, to, h.direction * speed, now);
  }

  /** Something else moved the scroller to `position`. A hold goes on from there; a press or a stop is dropped, as
   *  the page has been put somewhere on purpose. */
  adopt(position: number, _now: number): void {
    this.pos = clamp(position, 0, this.max);
    if (!this.held) this.stop();
  }

  /** Stops at once where the view is. */
  stop(): void {
    this.curve = null;
    this.held = null;
    this.settling = false;
    this.velocity = 0;
  }

  frame(now: number): { position: number; moving: boolean } {
    const h = this.held;
    if (h) {
      const gap = now - h.last;
      // A missing repeat is judged only on a frame that came on time: after a frame the page was too busy to draw,
      // the repeats queued meanwhile have not been delivered yet.
      const late = gap > 2 * MAX_FRAME_MS;
      const stall = h.intervals.length < MEASURED_INTERVALS ? MAX_STALL_MS : stallAfter(h.repeatMs);
      if (!late && now - h.lastRepeat > stall) {
        this.release(now);
        return this.frame(now);
      }
      const dt = clamp(gap, 0, MAX_FRAME_MS);
      const travel = this.travel(h, h.elapsed + dt) - this.travel(h, h.elapsed);
      h.elapsed += dt;
      h.last = now;
      const next = this.pos + h.direction * travel;
      if (h.direction > 0 ? next >= this.max : next <= 0) {
        this.pos = clamp(next, 0, this.max);
        this.stop();
        return { position: this.pos, moving: false };
      }
      this.pos = next;
      this.velocity = h.direction * this.speedAt(h, h.elapsed);
      return { position: this.pos, moving: true };
    }
    const c = this.curve;
    if (c) {
      if (now - c.start >= c.duration) {
        this.pos = clamp(c.to, 0, this.max);
        this.stop();
        return { position: this.pos, moving: false };
      }
      const at = this.curveAt(c, now);
      this.pos = clamp(at.position, 0, this.max);
      this.velocity = at.velocity;
      return { position: this.pos, moving: true };
    }
    return { position: this.pos, moving: false };
  }

  private startCurve(from: number, to: number, v: number, now: number): void {
    const distance = Math.abs(to - from);
    if (distance < 0.5) {
      this.pos = to;
      this.curve = null;
      this.settling = false;
      this.velocity = 0;
      return;
    }
    // A speed away from the target is not kept; a speed toward it sets how long the move takes, within bounds, and
    // is held to what a curve that never passes the target can start with.
    const toward = Math.sign(v) === Math.sign(to - from) ? v : 0;
    const duration = toward === 0 ? PRESS_MS : clamp((3 * distance) / Math.abs(toward), RETARGET_MIN_MS, RETARGET_MAX_MS);
    const v0 = Math.sign(toward) * Math.min(Math.abs(toward), (3 * distance) / duration);
    this.curve = { from, to, v0, duration, start: now };
    this.velocity = this.curveAt(this.curve, now).velocity;
  }

  /** The curve at `now`: a cubic ease-out from rest, or, leaving at `v0`, the cubic that starts at that speed and
   *  arrives at rest. */
  private curveAt(c: Curve, now: number): { position: number; velocity: number } {
    const u = clamp((now - c.start) / c.duration, 0, 1);
    const span = c.to - c.from;
    if (c.v0 === 0) {
      return { position: c.from + span * (1 - (1 - u) ** 3), velocity: (3 * span * (1 - u) ** 2) / c.duration };
    }
    const h01 = u * u * (3 - 2 * u);
    const h10 = u * (1 - u) * (1 - u);
    return {
      position: c.from + span * h01 + c.v0 * c.duration * h10,
      velocity: (span * 6 * u * (1 - u) + c.v0 * c.duration * (1 - u) * (1 - 3 * u)) / c.duration,
    };
  }

  private positionAt(now: number): number {
    return this.curve ? clamp(this.curveAt(this.curve, now).position, 0, this.max) : this.pos;
  }

  private velocityAt(now: number): number {
    if (this.curve) return now - this.curve.start >= this.curve.duration ? 0 : this.curveAt(this.curve, now).velocity;
    return this.held ? this.velocity : 0;
  }

  private speedAt(h: Hold, elapsed: number): number {
    const share = clamp((elapsed - h.rampAt) / RAMP_MS, 0, 1);
    return h.rampFrom + (h.target - h.rampFrom) * share;
  }

  /** The distance covered by hold time `elapsed` since the ramp began, exact for any frame times. */
  private travel(h: Hold, elapsed: number): number {
    const e = Math.max(0, elapsed - h.rampAt);
    const ramped = e < RAMP_MS ? (e * e) / (2 * RAMP_MS) : e - RAMP_MS / 2;
    return h.rampFrom * e + (h.target - h.rampFrom) * ramped;
  }
}

// ---------------------------------------------------------------------------------------------
// The DOM half: one glide at a time, on the window or on a scrollable box.
// ---------------------------------------------------------------------------------------------

/** `null`: the document's scrolling element, moved through the window. */
export type ScrollTargetEl = Element | null;

type GlideTrace = (f: { t: number; top: number; speed: number; phase: "press" | "hold" | "settle" }) => void;

declare global {
  interface Window {
    /** Set only by a measurement harness: receives one record per frame of a glide. */
    __eitriKeysScrollTrace?: GlideTrace;
  }
}

type Glide = {
  el: ScrollTargetEl;
  motion: GlideMotion;
  frame: number | null;
  lastWritten: number;
  /** The held key's `code` (or `key`) while a hold runs. */
  code: string | null;
};

let glide: Glide | null = null;

const SCROLLABLE = new Set(["auto", "scroll", "overlay"]);

/** The focused element's nearest scrollable ancestor (itself included), or `null` for the window. The walk stops
 *  before the body and the root: their overflow is the viewport's, and moving them moves nothing. */
export function scrollerOf(focused: Element | null): ScrollTargetEl {
  for (let el = focused; el; el = el.parentElement) {
    if (el === document.body || el === document.documentElement || el === document.scrollingElement) return null;
    if (SCROLLABLE.has(getComputedStyle(el).overflowY) && el.scrollHeight > el.clientHeight) return el;
  }
  return null;
}

/** The nearest ancestor of `from` (itself included) that scrolls sideways, or `null` for the window; the walk stops
 *  before the body and the root for the same reason as `scrollerOf`'s. */
export function hScrollerOf(from: Element | null): ScrollTargetEl {
  for (let el = from; el; el = el.parentElement) {
    if (el === document.body || el === document.documentElement || el === document.scrollingElement) return null;
    if (SCROLLABLE.has(getComputedStyle(el).overflowX) && el.scrollWidth > el.clientWidth) return el;
  }
  return null;
}

/** The furthest a sideways scroll goes; unbounded where the engine reports no scroll width. */
export function maxXOf(el: ScrollTargetEl): number {
  if (el !== null) return el.scrollWidth > 0 ? Math.max(0, el.scrollWidth - el.clientWidth) : Number.POSITIVE_INFINITY;
  const root = document.documentElement;
  const width = Math.max(root.scrollWidth, document.body?.scrollWidth ?? 0);
  const view = (document.scrollingElement ?? root).clientWidth || window.innerWidth;
  return width > 0 ? Math.max(0, width - view) : Number.POSITIVE_INFINITY;
}

/** The width `el` (`null`: the window) shows. */
export function viewWidthOf(el: ScrollTargetEl): number {
  return el === null ? (document.documentElement.clientWidth || window.innerWidth) : el.clientWidth;
}

/** Moves `el` (`null`: the window) sideways to `left` at once and returns where it is now. The vertical position and
 *  any glide are left alone, since a glide only writes the top. */
export function jumpX(el: ScrollTargetEl, left: number): number {
  const target = Math.round(clamp(left, 0, maxXOf(el)));
  if (el === null) {
    if (target !== Math.round(window.scrollX)) window.scrollTo({ left: target, behavior: "instant" });
    return target;
  }
  if (target !== Math.round(el.scrollLeft)) {
    if (typeof el.scrollTo === "function") el.scrollTo({ left: target, behavior: "instant" });
    else el.scrollLeft = target;
  }
  return target;
}

function topOf(el: ScrollTargetEl): number {
  return el === null ? window.scrollY : el.scrollTop;
}

/** The furthest the scroller goes; unbounded where the engine reports no scroll height. */
export function maxOf(el: ScrollTargetEl): number {
  if (el !== null) return el.scrollHeight > 0 ? Math.max(0, el.scrollHeight - el.clientHeight) : Number.POSITIVE_INFINITY;
  const root = document.documentElement;
  const height = Math.max(root.scrollHeight, document.body?.scrollHeight ?? 0);
  // The scrolling element's height is the viewport's in both modes; a quirks-mode root's is the whole document's.
  const view = (document.scrollingElement ?? root).clientHeight || window.innerHeight;
  return height > 0 ? Math.max(0, height - view) : Number.POSITIVE_INFINITY;
}

function viewOf(el: ScrollTargetEl): number {
  return el === null ? window.innerHeight : el.clientHeight;
}

function write(el: ScrollTargetEl, top: number): void {
  if (el === null) window.scrollTo({ top, behavior: "instant" });
  else if (typeof el.scrollTo === "function") el.scrollTo({ top, behavior: "instant" });
  else el.scrollTop = top;
}

/** Whether to jump instead of glide: the system asks for reduced motion (WebKitGTK follows GTK's animation
 *  setting), the user turned smoothing off, or there is no way to ask. */
function instant(): boolean {
  if (!currentSettings().smooth) return true;
  return typeof window.matchMedia !== "function" || window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** The glide on `el`, made at rest where `el` is if another scroller had the last one. Something else having moved
 *  the scroller since the last write is adopted here as well as on every frame. */
function glideOn(el: ScrollTargetEl, now: number): Glide {
  if (glide && glide.el !== el) cancelGlide();
  const top = topOf(el);
  if (!glide) {
    glide = { el, motion: new GlideMotion(top, maxOf(el)), frame: null, lastWritten: top, code: null };
  } else {
    glide.motion.setMax(maxOf(el));
    if (Math.abs(top - glide.lastWritten) > 1) {
      glide.motion.adopt(top, now);
      glide.lastWritten = top;
    }
  }
  glide.motion.step = currentSettings().scrollStep;
  return glide;
}

function tick(): void {
  const g = glide;
  if (!g) return;
  g.frame = null;
  const now = performance.now();
  const top = topOf(g.el);
  if (Math.abs(top - g.lastWritten) > 1) {
    g.motion.adopt(top, now);
    g.lastWritten = top;
  }
  g.motion.setMax(maxOf(g.el));
  const phase = g.motion.phase;
  const next = g.motion.frame(now);
  const rounded = Math.round(next.position);
  if (rounded !== g.lastWritten) {
    write(g.el, rounded);
    g.lastWritten = rounded;
  }
  const hook = window.__eitriKeysScrollTrace;
  if (typeof hook === "function" && phase !== "rest") {
    const shown = g.motion.phase === "rest" ? phase : g.motion.phase;
    hook({ t: now, top: rounded, speed: g.motion.speed, phase: shown as "press" | "hold" | "settle" });
  }
  if (g.motion.phase !== "hold") g.code = null;
  if (next.moving) g.frame = requestAnimationFrame(tick);
}

function run(g: Glide): void {
  if (g.frame === null && g.motion.moving) g.frame = requestAnimationFrame(tick);
}

/** Moves `el` (`null`: the window) to `top` at once, ending any glide. */
export function jump(el: ScrollTargetEl, top: number): void {
  cancelGlide();
  const target = Math.round(clamp(top, 0, maxOf(el)));
  if (target !== Math.round(topOf(el))) write(el, target);
}

const STEPS: Record<string, 1 | -1> = { "scroll.down": 1, "scroll.up": -1 };

/** Runs a `scroll.*` action for its key event (`null`: not from a key). False for any other action. */
export function glideAction(action: string, ev: KeyboardEvent | null): boolean {
  if (!action.startsWith("scroll.")) return false;
  const el = scrollerOf(document.activeElement);
  const { scrollStep } = currentSettings();
  const view = viewOf(el);
  const deltas: Record<string, number> = {
    "scroll.down": scrollStep,
    "scroll.up": -scrollStep,
    "scroll.half_down": view / 2,
    "scroll.half_up": -view / 2,
    "scroll.page_down": Math.max(scrollStep, view - 2 * scrollStep),
  };
  const absolute = action === "scroll.top" || action === "scroll.bottom";
  if (!absolute && !Object.hasOwnProperty.call(deltas, action)) return false;
  if (absolute && ev?.repeat) return true;
  const target = (from: number) => {
    if (action === "scroll.top") return 0;
    const max = maxOf(el);
    if (action === "scroll.bottom") return Number.isFinite(max) ? max : from;
    return from + deltas[action];
  };
  if (instant()) {
    jump(el, target(topOf(el)));
    return true;
  }
  const now = performance.now();
  const g = glideOn(el, now);
  const direction = STEPS[action];
  if (ev?.repeat && direction !== undefined) {
    g.motion.hold(direction, now);
    g.code = g.motion.phase === "hold" ? ev.code || ev.key : null;
  } else if (absolute) {
    g.motion.press(target(g.motion.position), now);
    g.code = null;
  } else {
    g.motion.pressBy(deltas[action], now);
    g.code = null;
  }
  run(g);
  return true;
}

/** The `code` (or `key`) of the key whose hold is running, or `null`. */
export function holdCode(): string | null {
  return glide?.motion.phase === "hold" ? glide.code : null;
}

/** Ends a running hold as a release does: the view eases to rest. Nothing else is touched. */
export function endHold(): void {
  if (!glide || glide.motion.phase !== "hold") return;
  glide.motion.release(performance.now());
  glide.code = null;
  run(glide);
}

/** Stops any glide at once where it is, with no coast. */
export function cancelGlide(): void {
  if (!glide) return;
  if (glide.frame !== null) cancelAnimationFrame(glide.frame);
  glide = null;
}

/** An eased move of `el` (`null`: the window) to `top`, or a jump where motion is unwanted. */
export function scrollToAnimated(el: ScrollTargetEl, top: number): void {
  if (instant()) {
    jump(el, top);
    return;
  }
  const now = performance.now();
  const g = glideOn(el, now);
  g.motion.press(top, now);
  g.code = null;
  run(g);
}

onRemove(cancelGlide);
