// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  GlideMotion, MAX_FRAME_MS, PRESS_MS, RAMP_MS, SETTLE_MS, cancelGlide, endHold, glideAction, scrollToAnimated,
  scrollerOf, setBottom, settleGlide, stallAfter, windowDestination,
} from "./glide";
import { DEFAULT_SETTINGS, useSettings } from "./settings";

type Write = { t: number; top: number; behavior: unknown };
type Frame = { t: number; top: number; speed: number; phase: string };

let y = 0;
let writes: Write[];
let frames: Frame[];
let reduced = false;

const key = (k: string, code: string, repeat = false) => new KeyboardEvent("keydown", { key: k, code, repeat });
const J = (repeat = false) => key("j", "KeyJ", repeat);
const K = (repeat = false) => key("k", "KeyK", repeat);
const D = (repeat = false) => key("d", "KeyD", repeat);

/** Runs `fn` at absolute fake time `t`. */
function at(t: number, fn: () => void): void {
  setTimeout(fn, t - performance.now());
}

/** Repeats of `action` every `every` ms from `every` up to `until`, each checked to write nothing itself. */
function repeats(action: string, ev: () => KeyboardEvent, every: number, until: number, during: number[] = []): void {
  for (let t = every; t <= until; t += every) {
    at(t, () => {
      const before = writes.length;
      glideAction(action, ev());
      during.push(writes.length - before);
    });
  }
}

function mean(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

function cv(xs: number[]): number {
  const m = mean(xs);
  return Math.sqrt(mean(xs.map((x) => (x - m) ** 2))) / m;
}

function stub(el: object, name: string, value: number): void {
  Object.defineProperty(el, name, { configurable: true, value });
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["requestAnimationFrame", "cancelAnimationFrame", "performance", "setTimeout", "clearTimeout"] });
  y = 0;
  writes = [];
  frames = [];
  reduced = false;
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => y });
  vi.spyOn(window, "scrollTo").mockImplementation(((opts: ScrollToOptions) => {
    writes.push({ t: performance.now(), top: opts.top!, behavior: opts.behavior });
    y = opts.top!;
  }) as typeof window.scrollTo);
  stub(document.documentElement, "scrollHeight", 100_000);
  stub(document.documentElement, "clientHeight", 800);
  stub(window, "innerHeight", 800);
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string) => ({ matches: reduced && query.includes("reduce") }),
  });
  window.__eitriKeysScrollTrace = (f) => { frames.push(f); };
});

afterEach(() => {
  cancelGlide();
  delete window.__eitriKeysScrollTrace;
  useSettings(DEFAULT_SETTINGS);
  vi.restoreAllMocks();
  vi.useRealTimers();
  for (const name of ["scrollHeight", "clientHeight"]) {
    delete (document.documentElement as unknown as Record<string, unknown>)[name];
    delete (document.body as unknown as Record<string, unknown>)[name];
  }
  delete (window as unknown as Record<string, unknown>).matchMedia;
  document.body.removeAttribute("style");
  document.body.replaceChildren();
});

/** Holds `j` from t = 0 with repeats every `every` ms until `until`. */
function holdJ(every: number, until: number, during: number[] = []): void {
  glideAction("scroll.down", J());
  repeats("scroll.down", () => J(true), every, until, during);
}

describe("presses", () => {
  it("one j rises monotonically from 0 to exactly the step, every write instant", () => {
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(PRESS_MS + 50);
    const tops = writes.map((w) => w.top);
    expect(tops.length).toBeGreaterThan(3);
    tops.forEach((top, i) => expect(top).toBeGreaterThan(i === 0 ? 0 : tops[i - 1]));
    expect(tops.at(-1)).toBe(60);
    expect(writes.every((w) => w.behavior === "instant")).toBe(true);
  });

  it("five j presses 20 ms apart end exactly five steps down", () => {
    for (let i = 0; i < 5; i++) at(i * 20, () => glideAction("scroll.down", J()));
    vi.advanceTimersByTime(800);
    expect(y).toBe(300);
    const tops = writes.map((w) => w.top);
    tops.forEach((top, i) => expect(top).toBeGreaterThanOrEqual(i === 0 ? 0 : tops[i - 1]));
  });

  it("the step follows scroll_step", () => {
    useSettings({ ...DEFAULT_SETTINGS, scrollStep: 40 });
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(400);
    expect(y).toBe(40);
  });

  it("<C-f> moves the view less two steps", () => {
    glideAction("scroll.page_down", key("f", "KeyF"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(680);
  });

  it("<C-b> moves the view back less two steps, and presses add up", () => {
    y = 2000;
    glideAction("scroll.page_up", key("b", "KeyB"));
    glideAction("scroll.page_up", key("b", "KeyB"));
    vi.advanceTimersByTime(600);
    expect(y).toBe(2000 - 2 * 680);
  });

  it("G goes where the page says its bottom is, held to the scroll range", () => {
    stub(document.documentElement, "scrollHeight", 2000);
    setBottom(() => 700);
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(700);
    setBottom(() => 5000);
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(1200);
    setBottom(() => null);
    y = 0;
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(1200);
    setBottom(() => { throw new Error("no"); });
    vi.spyOn(console, "warn").mockImplementation(() => {});
    y = 0;
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(1200);
    setBottom(null);
  });

  it("goes to the top and to the bottom of the document", () => {
    stub(document.documentElement, "scrollHeight", 2000);
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(1200);
    expect(writes.length).toBeGreaterThan(2);
    glideAction("scroll.top", key("g", "KeyG"));
    vi.advanceTimersByTime(400);
    expect(y).toBe(0);
  });

  it("ignores a repeat of G", () => {
    stub(document.documentElement, "scrollHeight", 2000);
    glideAction("scroll.bottom", key("G", "KeyG"));
    vi.advanceTimersByTime(400);
    const count = writes.length;
    expect(glideAction("scroll.bottom", key("G", "KeyG", true))).toBe(true);
    vi.advanceTimersByTime(400);
    expect(writes).toHaveLength(count);
  });

  it("with smooth off every scroll key writes its whole move at once", () => {
    useSettings({ ...DEFAULT_SETTINGS, smooth: false });
    const raf = vi.spyOn(window, "requestAnimationFrame");
    glideAction("scroll.down", J());
    expect(writes.map((w) => w.top)).toEqual([60]);
    glideAction("scroll.half_down", D());
    expect(writes.map((w) => w.top)).toEqual([60, 460]);
    glideAction("scroll.down", J(true));
    expect(writes.map((w) => w.top)).toEqual([60, 460, 520]);
    glideAction("scroll.top", key("g", "KeyG"));
    expect(writes.map((w) => w.top)).toEqual([60, 460, 520, 0]);
    vi.advanceTimersByTime(400);
    expect(writes).toHaveLength(4);
    expect(writes.every((w) => w.behavior === "instant")).toBe(true);
    expect(raf).not.toHaveBeenCalled();
  });

  it("with prefers-reduced-motion each keydown writes one step instantly and no frame runs", () => {
    reduced = true;
    const raf = vi.spyOn(window, "requestAnimationFrame");
    glideAction("scroll.down", J());
    expect(writes.map((w) => w.top)).toEqual([60]);
    glideAction("scroll.down", J(true));
    glideAction("scroll.down", J(true));
    expect(writes.map((w) => w.top)).toEqual([60, 120, 180]);
    vi.advanceTimersByTime(500);
    expect(writes).toHaveLength(3);
    expect(raf).not.toHaveBeenCalled();
  });

  it("leaves other actions alone", () => {
    expect(glideAction("history.back", null)).toBe(false);
    expect(glideAction("scroll.sideways", null)).toBe(false);
  });

  it("windowDestination is where a running press is heading, and where the window is at rest", () => {
    expect(windowDestination()).toBe(0);
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(30);
    expect(y).toBeGreaterThan(0);
    expect(y).toBeLessThan(60);
    expect(windowDestination()).toBe(60);
    glideAction("scroll.down", J());
    expect(windowDestination()).toBe(120);
    vi.advanceTimersByTime(400);
    expect(windowDestination()).toBe(120);
    y = 333;
    expect(windowDestination()).toBe(333);
  });

  it("settleGlide ends a press at its destination at once, and a hold where its release comes to rest", () => {
    glideAction("scroll.half_down", D());
    vi.advanceTimersByTime(30);
    expect(y).toBeLessThan(400);
    settleGlide();
    expect(y).toBe(400);
    const count = writes.length;
    vi.advanceTimersByTime(400);
    expect(writes).toHaveLength(count);
    holdJ(33, 240);
    vi.advanceTimersByTime(250);
    const before = y;
    settleGlide();
    expect(y).toBeGreaterThanOrEqual(before);
    const settled = y;
    vi.advanceTimersByTime(400);
    expect(y).toBe(settled);
    settleGlide();
    expect(y).toBe(settled);
  });

  it("scrollToAnimated eases the window to a place", () => {
    scrollToAnimated(null, 500);
    vi.advanceTimersByTime(400);
    expect(y).toBe(500);
    expect(writes.length).toBeGreaterThan(2);
  });
});

describe("a held key", () => {
  it("a held j glides: no still frame after the ramp, per-frame moves with cv below 0.1, mean speed step/33 within 10%, no dip at a repeat, one write per frame and none per keydown", () => {
    const during: number[] = [];
    holdJ(33, 990, during);
    vi.advanceTimersByTime(1000);
    expect(during.length).toBe(30);
    expect(during.every((n) => n === 0)).toBe(true);
    const times = writes.map((w) => w.t);
    expect(new Set(times).size).toBe(times.length);
    const steady = frames.filter((f) => f.t >= 33 + RAMP_MS + 20 && f.t <= 990);
    expect(steady.length).toBeGreaterThan(40);
    expect(steady.every((f) => f.phase === "hold")).toBe(true);
    // Every frame of the steady glide wrote a new position.
    for (const f of steady) expect(writes.some((w) => w.t === f.t)).toBe(true);
    const moves = steady.slice(1).map((f, i) => f.top - steady[i].top);
    expect(Math.min(...moves)).toBeGreaterThan(0);
    expect(cv(moves)).toBeLessThan(0.1);
    expect(Math.min(...moves)).toBeGreaterThanOrEqual(0.85 * mean(moves));
    const speed = (steady.at(-1)!.top - steady[0].top) / (steady.at(-1)!.t - steady[0].t);
    expect(Math.abs(speed - 60 / 33) / (60 / 33)).toBeLessThan(0.1);
  });

  it("the hold speed follows the median of the first three repeat intervals", () => {
    holdJ(50, 1000);
    vi.advanceTimersByTime(1000);
    const window = frames.filter((f) => f.t >= 450 && f.t <= 950);
    const speed = (window.at(-1)!.top - window[0].top) / (window.at(-1)!.t - window[0].t);
    expect(Math.abs(speed - 60 / 50) / (60 / 50)).toBeLessThan(0.1);
  });

  it("keyup decelerates monotonically, never reverses, and stops within the settle time", () => {
    holdJ(33, 495);
    at(500, () => endHold());
    vi.advanceTimersByTime(1000);
    const after = frames.filter((f) => f.t > 500);
    expect(after.length).toBeGreaterThan(2);
    expect(after.every((f) => f.phase === "settle")).toBe(true);
    const before = frames.filter((f) => f.t <= 500).at(-1)!;
    let speed = before.speed;
    let top = before.top;
    for (const f of after) {
      expect(f.speed).toBeLessThanOrEqual(speed + 1e-9);
      expect(f.speed).toBeGreaterThanOrEqual(0);
      expect(f.top).toBeGreaterThanOrEqual(top);
      speed = f.speed;
      top = f.top;
    }
    expect(writes.at(-1)!.t).toBeLessThanOrEqual(500 + SETTLE_MS + 16);
  });

  it("repeats that stop without a keyup end the hold after the stall window; a 100 ms late frame neither ends it nor leaps more than MAX_FRAME_MS of speed", () => {
    // Frames every 16 ms; each repeat due by a frame's time is delivered before that frame.
    const drive = (m: GlideMotion, from: number, to: number, due: number[]) => {
      const out: { t: number; position: number; phase: string }[] = [];
      for (let t = from + 16; t <= to; t += 16) {
        while (due.length && due[0] <= t) m.hold(1, due.shift()!);
        out.push({ t, ...m.frame(t), phase: m.phase });
      }
      return out;
    };
    const every = (step: number, from: number, to: number) => {
      const out: number[] = [];
      for (let t = from; t <= to; t += step) out.push(t);
      return out;
    };

    const stalled = new GlideMotion(0, 1e6);
    stalled.hold(1, 0);
    const run = drive(stalled, 0, 700, every(33, 33, 330));
    const ended = run.find((f) => f.phase !== "hold")!;
    expect(ended.t).toBeGreaterThanOrEqual(330 + stallAfter(33));
    expect(ended.t).toBeLessThanOrEqual(330 + stallAfter(33) + 16);

    const late = new GlideMotion(0, 1e6);
    late.hold(1, 0);
    const first = drive(late, 0, 320, every(33, 33, 320));
    expect(first.at(-1)!.phase).toBe("hold");
    const speed = late.speed;
    const before = late.position;
    // No frame and no repeat for 100 ms: the page was busy.
    const leap = late.frame(420);
    expect(late.phase).toBe("hold");
    expect(leap.position - before).toBeLessThanOrEqual(MAX_FRAME_MS * speed + 1e-6);
    expect(leap.position - before).toBeGreaterThan(0);
    // The queued repeats arrive; the glide carries on.
    const rest = drive(late, 420, 600, every(33, 421, 600));
    expect(rest.every((f) => f.phase === "hold")).toBe(true);
  });

  it("an external scrollTo mid-hold is adopted and the glide goes on from there; mid-press the press is cancelled", () => {
    holdJ(33, 700);
    at(408, () => { y = 5000; });
    vi.advanceTimersByTime(700);
    const after = writes.filter((w) => w.t > 408);
    expect(after[0].top).toBeGreaterThan(5000);
    expect(after[0].top).toBeLessThan(5000 + 40);
    after.slice(1).forEach((w, i) => expect(w.top).toBeGreaterThan(after[i].top));
    cancelGlide();

    y = 0;
    writes = [];
    const t0 = performance.now();
    glideAction("scroll.down", J());
    at(t0 + 40, () => { y = 3000; });
    vi.advanceTimersByTime(400);
    expect(writes.filter((w) => w.t > t0 + 40)).toEqual([]);
    expect(y).toBe(3000);
  });

  it("at the bottom the hold ends and writes stop; k at the top likewise", () => {
    stub(document.documentElement, "scrollHeight", 2000);
    y = 1100;
    holdJ(33, 990);
    vi.advanceTimersByTime(1000);
    expect(writes.at(-1)!.top).toBe(1200);
    expect(writes.filter((w) => w.top === 1200)).toHaveLength(1);
    expect(writes.every((w) => w.top <= 1200)).toBe(true);
    cancelGlide();

    y = 0;
    writes = [];
    const t0 = performance.now();
    glideAction("scroll.up", K());
    for (let t = 33; t <= 300; t += 33) at(t0 + t, () => glideAction("scroll.up", K(true)));
    vi.advanceTimersByTime(400);
    expect(writes).toEqual([]);
  });

  it("a held d adds half a page to the destination per repeat, monotonic, velocity never reset", () => {
    glideAction("scroll.half_down", D());
    repeats("scroll.half_down", () => D(true), 33, 330);
    vi.advanceTimersByTime(1500);
    expect(y).toBe(11 * 400);
    const tops = writes.map((w) => w.top);
    tops.forEach((top, i) => expect(top).toBeGreaterThan(i === 0 ? 0 : tops[i - 1]));
    const moving = frames.filter((f) => f.t <= 330);
    expect(moving.every((f) => f.speed > 0)).toBe(true);
  });

  it("gg during a j hold ends the hold and animates to 0", () => {
    holdJ(33, 390);
    at(400, () => glideAction("scroll.top", key("g", "KeyG")));
    vi.advanceTimersByTime(800);
    expect(y).toBe(0);
    const after = writes.filter((w) => w.t > 400);
    expect(after.length).toBeGreaterThan(2);
    after.slice(1).forEach((w, i) => expect(w.top).toBeLessThan(after[i].top));
  });

  it("k during a steady j hold starts from where the page is: no frame after the k is further down than the frame before it, and the page comes to rest one step above where it was when the k came", () => {
    holdJ(33, 495);
    let when = 0;
    at(500, () => {
      when = y;
      endHold();
      glideAction("scroll.up", K());
    });
    vi.advanceTimersByTime(800);
    const after = writes.filter((w) => w.t > 500);
    expect(when).toBeGreaterThan(500);
    let previous = when;
    for (const w of after) {
      expect(w.top).toBeLessThanOrEqual(previous);
      previous = w.top;
    }
    expect(y).toBe(when - 60);
  });
});

describe("which element scrolls", () => {
  it("j inside a focused scrollable box scrolls that box, not the window", () => {
    const box = document.createElement("div");
    box.style.overflowY = "auto";
    box.tabIndex = 0;
    const item = document.createElement("button");
    box.append(item);
    document.body.append(box);
    stub(box, "scrollHeight", 2000);
    stub(box, "clientHeight", 400);
    let top = 0;
    Object.defineProperty(box, "scrollTop", { configurable: true, get: () => top, set: (v: number) => { top = v; } });
    const boxWrites: number[] = [];
    box.scrollTo = ((opts: ScrollToOptions) => { boxWrites.push(opts.top!); top = opts.top!; }) as typeof box.scrollTo;
    item.focus();
    expect(scrollerOf(item)).toBe(box);
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(400);
    expect(top).toBe(60);
    expect(boxWrites.length).toBeGreaterThan(1);
    expect(writes).toEqual([]);
    glideAction("scroll.half_down", D());
    vi.advanceTimersByTime(400);
    expect(top).toBe(260);
  });

  it("j glides the window of a quirks-mode page, whose root is as tall as the document", () => {
    stub(document.documentElement, "clientHeight", 100_000);
    stub(document.body, "clientHeight", 800);
    Object.defineProperty(document, "scrollingElement", { configurable: true, get: () => document.body });
    try {
      glideAction("scroll.down", J());
      vi.advanceTimersByTime(400);
      expect(y).toBe(60);
    } finally {
      delete (document as unknown as Record<string, unknown>).scrollingElement;
    }
  });

  it("j with focus on an item under a body with overflow-x hidden and 1 px to spare scrolls the window", () => {
    document.body.style.overflowX = "hidden";
    document.body.style.overflowY = "auto";
    stub(document.body, "clientHeight", 800);
    stub(document.body, "scrollHeight", 801);
    const item = document.createElement("button");
    document.body.append(item);
    item.focus();
    expect(scrollerOf(item)).toBeNull();
    expect(scrollerOf(document.body)).toBeNull();
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(400);
    expect(y).toBe(60);
  });
});

describe("the trace hook", () => {
  it("the trace hook sees one record per frame when set, nothing when unset", () => {
    const raf = vi.spyOn(window, "requestAnimationFrame");
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(400);
    expect(frames.length).toBeGreaterThan(3);
    expect(frames).toHaveLength(raf.mock.calls.length);
    expect(frames.every((f) => f.phase === "press")).toBe(true);
    delete window.__eitriKeysScrollTrace;
    frames = [];
    glideAction("scroll.down", J());
    vi.advanceTimersByTime(400);
    expect(frames).toEqual([]);
    expect(y).toBe(120);
  });
});
