import { describe, expect, it } from "vitest";
import { ScrollGuard } from "./anchor";
import { GUARD_MS } from "./sync";

describe("ScrollGuard", () => {
  it("takes the scroll events a programmatic scroll causes as its own, until it settles or 150 ms pass", () => {
    let now = 1000;
    const frames: (() => void)[] = [];
    const guard = new ScrollGuard(() => now, (fn) => frames.push(fn));
    expect(guard.onScroll(0)).toBe(false);
    guard.arm(400);
    expect(guard.onScroll(250)).toBe(true);
    expect(guard.onScroll(400)).toBe(true);
    expect(frames.length).toBe(1);
    frames.shift()!();
    expect(guard.onScroll(410)).toBe(false);
    guard.arm(100);
    now += GUARD_MS;
    expect(guard.onScroll(300)).toBe(false);
  });
  it("is not cleared by a frame from an earlier arm", () => {
    const frames: (() => void)[] = [];
    const guard = new ScrollGuard(() => 0, (fn) => frames.push(fn));
    guard.arm(10);
    guard.onScroll(10);
    guard.arm(50);
    frames.shift()!();
    expect(guard.onScroll(30)).toBe(true);
  });
});
