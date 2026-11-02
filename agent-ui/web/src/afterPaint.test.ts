// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

vi.unmock("./afterPaint");
import { afterPaint } from "./afterPaint";

afterEach(() => vi.unstubAllGlobals());

function frames() {
  const queue: FrameRequestCallback[] = [];
  vi.stubGlobal("requestAnimationFrame", (run: FrameRequestCallback) => queue.push(run));
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    queue[id - 1] = () => {};
  });
  return { next: () => queue.shift()?.(0), pending: () => queue.length };
}

describe("afterPaint", () => {
  it("waits for the frame after the one that paints", () => {
    const clock = frames();
    const run = vi.fn();
    afterPaint(run);
    clock.next();
    expect(run).not.toHaveBeenCalled();
    clock.next();
    expect(run).toHaveBeenCalledTimes(1);
  });

  it("runs nothing once called off, in either frame", () => {
    const clock = frames();
    const run = vi.fn();
    afterPaint(run)();
    clock.next();
    clock.next();
    expect(run).not.toHaveBeenCalled();
  });
});
