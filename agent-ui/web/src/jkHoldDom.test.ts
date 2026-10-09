// @vitest-environment jsdom
/** The held `j`/`k` glide's driver (`startHold`, `keepHoldAlive`) on a list whose layout follows the DOM: rows
 *  above the cursor going away mid-hold, and a slow key-repeat rate. */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { isHolding, keepHoldAlive, releaseHold, startHold } from "./jkHold";
import { conversationRows } from "./nav";

const VIEW = 400;
let now = 0;
let nextId = 1;
let queue = new Map<number, FrameRequestCallback>();

function frames(ms: number) {
  const end = now + ms;
  while (now < end) {
    now = Math.min(end, now + 16);
    const due = [...queue.values()];
    queue = new Map();
    for (const callback of due) callback(now);
  }
}

/** A list of rows `heights` tall, laid out from the DOM's own order each time a rect is read. */
function list(heights: number[]): HTMLElement {
  const el = document.createElement("div");
  el.className = "message-list";
  document.body.appendChild(el);
  const height = new Map<Element, number>();
  for (const h of heights) {
    const row = document.createElement("div");
    row.setAttribute("data-nav-stop", "row");
    row.id = `r${height.size}`;
    row.innerHTML = '<div class="row-body" style="line-height: 20px">x</div>';
    height.set(row, h);
    el.appendChild(row);
  }
  let scrollTop = 0;
  const total = () => Array.from(el.children).reduce((sum, row) => sum + (height.get(row) ?? 0), 0);
  Object.defineProperty(el, "clientHeight", { value: VIEW, configurable: true });
  Object.defineProperty(el, "scrollHeight", { get: total, configurable: true });
  Object.defineProperty(el, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(total() - VIEW, v));
    },
  });
  el.getBoundingClientRect = () => ({ top: 0, bottom: VIEW }) as DOMRect;
  for (const row of Array.from(el.children) as HTMLElement[]) {
    row.getBoundingClientRect = () => {
      let y = 0;
      for (const other of Array.from(el.children)) {
        if (other === row) break;
        y += height.get(other) ?? 0;
      }
      return { top: y - scrollTop, bottom: y + (height.get(row) ?? 0) - scrollTop } as DOMRect;
    };
  }
  return el;
}

beforeEach(() => {
  now = 1000;
  queue = new Map();
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    const id = nextId++;
    queue.set(id, cb);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => queue.delete(id));
  vi.spyOn(performance, "now").mockImplementation(() => now);
  (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: false });
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  delete (window as unknown as { matchMedia?: unknown }).matchMedia;
  document.body.innerHTML = "";
});

/** Holds `j` on `el` for `ms`, a repeat every `every` ms, recording the cursor's rows as elements. */
function hold(el: HTMLElement, ms: number, every: number, seen: HTMLElement[]) {
  for (let t = 0; t < ms; t += every) {
    keepHoldAlive(el, "KeyJ", now);
    frames(every);
  }
  return seen;
}

describe("the glide's driver", () => {
  it("follows the cursor's row as an element when rows above it go away, never skipping a row", () => {
    const el = list(Array.from({ length: 80 }, () => 60));
    const seen: HTMLElement[] = [];
    const started = startHold({
      list: el,
      rows: () => conversationRows(el),
      cursor: 2,
      direction: 1,
      code: "KeyJ",
      now,
      onCursor: (index) => seen.push(conversationRows(el)[index]),
      onRelease: () => {},
    });
    expect(started).toBe(true);
    hold(el, 320, 32, seen);
    const before = seen.length;
    // Two rows above the cursor go away (cards answered, calls folded into a run).
    const rows = conversationRows(el);
    rows[0].remove();
    rows[1].remove();
    hold(el, 900, 32, seen);
    releaseHold(el, now);
    const order = Array.from(el.children);
    for (let i = 1; i < seen.length; i++) {
      expect(order.indexOf(seen[i]) - order.indexOf(seen[i - 1])).toBe(1);
    }
    // It was still gliding on both sides of the change.
    expect(before).toBeGreaterThan(1);
    expect(seen.length - before).toBeGreaterThan(5);
  });

  it("keeps gliding at a slow key-repeat rate, never ending between two repeats", () => {
    const el = list(Array.from({ length: 40 }, () => 60));
    const started = startHold({
      list: el,
      rows: () => conversationRows(el),
      cursor: 1,
      direction: 1,
      code: "KeyJ",
      now,
      onCursor: () => {},
      onRelease: () => {},
    });
    expect(started).toBe(true);
    for (let t = 0; t < 1000; t += 100) {
      keepHoldAlive(el, "KeyJ", now);
      frames(96);
      expect(isHolding(el)).toBe(true);
      frames(4);
    }
    releaseHold(el, now);
  });
});
