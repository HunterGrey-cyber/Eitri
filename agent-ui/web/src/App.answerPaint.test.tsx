// @vitest-environment jsdom
/**
 * A key answers a y/n question only if it was pressed after the question was painted. These tests take the real
 * `afterPaint` back from `testSetup.ts`, with animation frames under fake timers, and run on a clock of their own that
 * only they move. A render moves it too: every `revealHidden` call spends time on it, so the slow commit is a real
 * commit of real rows, with no timer running meanwhile, as on a busy main thread.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";

vi.unmock("./afterPaint");
const clock = vi.hoisted(() => ({ now: 1_000, msPerLine: 0 }));
vi.mock("./revealHidden", async (original) => {
  const real = await original<typeof import("./revealHidden")>();
  return {
    ...real,
    revealHidden: (text: string) => {
      clock.now += clock.msPerLine;
      return real.revealHidden(text);
    },
  };
});

import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import type { AgentUiState, Hello } from "./types";

const FRAME_MS = 17;
const ROWS = 2_000;
const MS_PER_LINE = 1;
const HEX = "a".repeat(64);
const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

let posted: Array<Record<string, unknown>>;

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  clock.msPerLine = 0;
});
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});
beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"] });
  clock.now = 1_000;
  vi.spyOn(performance, "now").mockImplementation(() => clock.now);
  vi.stubGlobal("ResizeObserver", class {
    observe() {}
    unobserve() {}
    disconnect() {}
  });
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

function mount() {
  const view = render(<App />);
  const state: AgentUiState = { ...initialState(), status: { kind: "running" } };
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  pass(1_000);
  return view;
}

/** Time passes with the page idle: timers and frames run. */
const pass = (ms: number) => {
  clock.now += ms;
  act(() => vi.advanceTimersByTime(ms));
};
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;

/** A keydown pressed at `at` and handled now, as the browser hands over a key it queued while the page was busy. */
function press(c: HTMLElement, key: string, at: number) {
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true });
  Object.defineProperty(event, "timeStamp", { value: at });
  act(() => {
    root(c).dispatchEvent(event);
  });
}

function trustPrompt(rows: number) {
  return {
    kind: "trust_prompt", tab: 1, nonce: 9, agent: "Claude", root: "/p", top: "/p", fingerprint: HEX, findingsDigest: HEX,
    state: "changed", remember: "yes", rememberNote: null, items: [],
    changed: {
      added: [], removed: [], changed: ["CLAUDE.md"],
      texts: [{
        kind: "diff", path: "CLAUDE.md", added: rows, removed: 0, skippedAfter: 0,
        hunks: [{ skipped: 0, lines: Array.from({ length: rows }, (_, i) => ({ kind: "added", text: `line ${i}` })) }],
      }],
    },
  };
}

const frames = (n: number) => pass(n * FRAME_MS);
const answers = () => posted.filter((m) => m.type === "trust_answer");

describe("a key pressed before the question was painted", () => {
  it("never answers it, however long the render took", () => {
    const { container } = mount();
    clock.msPerLine = MS_PER_LINE;
    const asked = clock.now;
    dispatch(trustPrompt(ROWS));
    clock.msPerLine = 0;
    expect(clock.now - asked).toBeGreaterThanOrEqual(ROWS * MS_PER_LINE);
    expect(container.querySelectorAll(".trust-text-diff .diff-line")).toHaveLength(ROWS);

    // Pressed in the middle of the render, handled after its commit, well past 250 ms after the question came.
    press(container, "y", asked + 300);
    expect(answers()).toHaveLength(0);

    // The frames come. The next queued key is still from before them, and so is the one after.
    frames(2);
    press(container, "n", asked + 700);
    expect(answers()).toHaveLength(0);
    pass(100);
    press(container, "y", asked + 1_300);
    expect(answers()).toHaveLength(0);
  });

  it("counts once 250 ms have passed since the paint", () => {
    const { container } = mount();
    dispatch(trustPrompt(3));
    frames(2);
    const painted = clock.now;
    pass(100);
    press(container, "y", clock.now);
    expect(answers()).toHaveLength(0);
    pass(400);
    press(container, "y", painted + 300);
    expect(answers()).toHaveLength(0);
    pass(400);
    press(container, "y", clock.now);
    expect(answers()).toHaveLength(1);
  });

  it("starts over for each asking", () => {
    const { container } = mount();
    dispatch(trustPrompt(3));
    frames(2);
    pass(1_000);
    // The next question has not been painted. The one before it has been up for a second, which says nothing about it.
    dispatch({ ...trustPrompt(3), nonce: 10 });
    press(container, "y", clock.now);
    expect(answers()).toHaveLength(0);
  });

  it("takes no answer from the key that turns a clock offset on, however it times out", () => {
    const { container } = mount();
    const hour = 3_600_000;
    clock.now += 2 * hour;
    dispatch(trustPrompt(3));
    frames(2);
    pass(1_000);
    press(container, "k", clock.now - hour);
    pass(51);
    press(container, "y", clock.now - hour);
    expect(answers()).toHaveLength(0);
    pass(1_000);
    press(container, "y", clock.now - hour);
    expect(answers()).toHaveLength(1);
  });

  it("is not answered in a view that gets no frames", () => {
    const { container } = mount();
    vi.stubGlobal("requestAnimationFrame", () => 0);
    dispatch(trustPrompt(3));
    pass(5_000);
    press(container, "y", clock.now);
    expect(answers()).toHaveLength(0);
  });

  it("is the same guard for the bypass prompt", () => {
    const { container } = mount();
    const asked = clock.now;
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["Bypass permissions? y/n"] });
    clock.now += 500;
    press(container, "y", asked + 400);
    expect(posted.filter((m) => m.type === "confirm_bypass")).toHaveLength(0);
  });

  it("leaves the bypass prompt answerable once it has been on screen", () => {
    const { container } = mount();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["Bypass permissions? y/n"] });
    frames(2);
    pass(300);
    press(container, "y", clock.now);
    expect(posted.filter((m) => m.type === "confirm_bypass")).toHaveLength(1);
  });
});
