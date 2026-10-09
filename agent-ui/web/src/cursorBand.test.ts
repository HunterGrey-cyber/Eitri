// @vitest-environment jsdom
/** The cursor's band (`cursorBand.ts`): where it is drawn, the glide from one row to the next, and no
 *  glide where motion is not wanted. */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CURSOR_GLIDE_MS, CursorBand } from "./cursorBand";

let now = 0;
let queue = new Map<number, FrameRequestCallback>();
let nextId = 1;

function frames(ms: number) {
  const end = now + ms;
  while (now < end) {
    now = Math.min(end, now + 16);
    const due = [...queue.values()];
    queue = new Map();
    for (const callback of due) callback(now);
  }
}

const top = (el: HTMLElement) => Number(/translate\([^,]+, ([-\d.]+)px\)/.exec(el.style.transform)![1]);

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
});

const ROW_A = { left: 0, top: 0, width: 400, height: 40 };
const ROW_B = { left: 0, top: 200, width: 400, height: 120 };

describe("CursorBand", () => {
  it("is placed at once the first time, then glides to the next row over about 100ms, eased", () => {
    const el = document.createElement("div");
    const band = new CursorBand(el);
    band.place(ROW_A, true);
    expect(top(el)).toBe(0);
    expect(el.style.height).toBe("40px");
    band.place(ROW_B, true);
    expect(top(el)).toBe(0);
    frames(16);
    const early = top(el);
    expect(early).toBeGreaterThan(0);
    expect(early).toBeLessThan(200);
    // Eased out: the first frame covers more than its share of the time.
    expect(early / 200).toBeGreaterThan(16 / CURSOR_GLIDE_MS);
    frames(CURSOR_GLIDE_MS);
    expect(top(el)).toBe(200);
    expect(el.style.height).toBe("120px");
    expect(queue.size).toBe(0);
  });

  it("picks up from where it is when the cursor moves again mid-glide", () => {
    const el = document.createElement("div");
    const band = new CursorBand(el);
    band.place(ROW_A, true);
    band.place(ROW_B, true);
    frames(32);
    const mid = top(el);
    band.place({ ...ROW_B, top: 500 }, true);
    frames(16);
    expect(top(el)).toBeGreaterThan(mid);
    frames(CURSOR_GLIDE_MS);
    expect(top(el)).toBe(500);
  });

  it("moves at once without a glide asked for (a row resized), or with reduced motion", () => {
    const el = document.createElement("div");
    const band = new CursorBand(el);
    band.place(ROW_A, true);
    band.place(ROW_B, false);
    expect(top(el)).toBe(200);
    (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: true });
    band.place(ROW_A, true);
    expect(top(el)).toBe(0);
    expect(queue.size).toBe(0);
  });

  it("hides with no row and comes back where its row is, with no glide from where it was", () => {
    const el = document.createElement("div");
    const band = new CursorBand(el);
    band.place(ROW_A, true);
    band.place(null, true);
    expect(el.dataset.row).toBe("none");
    band.place(ROW_B, true);
    expect(el.dataset.row).toBeUndefined();
    expect(top(el)).toBe(200);
  });
});
