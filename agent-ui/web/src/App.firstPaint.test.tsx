// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import type { Hello } from "./types";

let posted: Array<Record<string, unknown>>;
let frames: Map<number, FrameRequestCallback>;
let nextFrame: number;

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  vi.useFakeTimers();
  posted = [];
  frames = new Map();
  nextFrame = 0;
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    const id = ++nextFrame;
    frames.set(id, callback);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("ResizeObserver", class {
    observe() {}
    unobserve() {}
    disconnect() {}
  });
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn() }, configurable: true });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function flushFrame() {
  act(() => {
    const pending = [...frames.values()];
    frames.clear();
    for (const callback of pending) callback(performance.now());
  });
}

const painted = () => posted.filter((message) => message.type === "painted");
const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/project",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};

describe("first paint", () => {
  it("posts once after two frames, independently of envelopes and re-renders", () => {
    const { rerender } = render(<App />);
    expect(painted()).toEqual([]);
    flushFrame();
    expect(painted()).toEqual([]);
    flushFrame();
    expect(painted()).toEqual([{ type: "painted" }]);

    act(() => window.__eitriDispatch!(JSON.stringify({ kind: "hello", ...HELLO })));
    rerender(<App />);
    flushFrame();
    flushFrame();
    act(() => vi.advanceTimersByTime(10_000));
    flushFrame();
    expect(painted()).toEqual([{ type: "painted" }]);
  });

  it.each([0, 1])("cancels the pending paint notification on unmount after %i frames", (count) => {
    const { unmount } = render(<App />);
    for (let i = 0; i < count; i++) flushFrame();
    unmount();
    flushFrame();
    flushFrame();
    expect(painted()).toEqual([]);
  });
});
