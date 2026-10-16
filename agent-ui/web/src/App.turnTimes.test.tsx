// @vitest-environment jsdom
/**
 * "Worked for 8s" under a finished turn: the active tab's `turnTimes` replace the panel's table on a snapshot and
 * grow with the turns an events envelope ends. Placement and chrome rules are `MessageList.turnTime.test.tsx`.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello } from "./types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers();
  widen = stubBandWidth();
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn() }, configurable: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return (container: HTMLElement) => {
    const band = container.querySelector(".status-band");
    const measure = container.querySelector(".band-measure");
    for (const o of observers) {
      for (const el of o.observed) {
        if (el === band) o.callback([{ target: el, contentRect: { width: 1400 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}
let widen: (container: HTMLElement) => void;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));

/** Tab 1 live, the pane focused and the keys arrived, in BROWSE. */
function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}


const lines = (c: HTMLElement) => Array.from(c.querySelectorAll(".turn-time")).map((el) => el.textContent);
const turn = (id: string, ms: number, anchorSeq: number | null) => ({ turnId: id, startedAtMs: 5_000, endedAtMs: 5_000 + ms, anchorSeq });

const twoTurnState = () =>
  snapshotState({
    userPrompts: [{ seq: 1, text: "one" }, { seq: 3, text: "two" }],
    transcript: [{ seq: 2, text: "reply one" }, { seq: 4, text: "reply two" }],
  });

describe("Worked for", () => {
  it("a snapshot's turnTimes draw a line under each finished turn, and the next snapshot replaces them", () => {
    const { container } = started();
    dispatch({
      kind: "snapshot", tab: 1, throughRevision: 5, state: twoTurnState(),
      turnTimes: [turn("t1", 42_000, 2), turn("t2", 65_000, 4)],
    });
    expect(lines(container)).toEqual(["Worked for 42s", "Worked for 1m 05s"]);
    expect(container.querySelectorAll("[data-nav-stop]")).toHaveLength(4);

    dispatch({ kind: "snapshot", tab: 1, throughRevision: 6, state: twoTurnState(), turnTimes: [turn("t2", 9_000, 4)] });
    expect(lines(container)).toEqual(["Worked for 9s"]);

    // A snapshot from a build that sends none knows no times.
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 7, state: twoTurnState() });
    expect(lines(container)).toEqual([]);
  });

  it("an events envelope adds the turns that ended in it", () => {
    const { container } = started();
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state: twoTurnState(), turnTimes: [turn("t1", 42_000, 2)] });
    expect(lines(container)).toEqual(["Worked for 42s"]);
    dispatch({
      kind: "events", tab: 1, fromRevision: 5, throughRevision: 8,
      events: [
        { type: "turn_started", turn_id: "t3" },
        { type: "user_prompt_submitted", text: "three" },
        { type: "content_delta", turn_id: "t3", kind: "text", text: "reply three" },
      ],
    });
    expect(lines(container)).toEqual(["Worked for 42s"]);
    dispatch({
      kind: "events", tab: 1, fromRevision: 8, throughRevision: 9,
      events: [{ type: "turn_completed", turn_id: "t3", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } }],
      turnTimes: [turn("t3", 83_000, 7)],
    });
    expect(lines(container)).toEqual(["Worked for 42s", "Worked for 1m 23s"]);
  });
});
