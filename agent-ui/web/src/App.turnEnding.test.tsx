// @vitest-environment jsdom
/**
 * A turn that does not complete -- failed, hit a limit, interrupted, or cut off by the session ending -- is a muted
 * row where it stopped and a word in the status band until the next turn starts, whether the events arrived live or
 * the panel was rebuilt from a snapshot. Both payloads come from `fixtures/turn-endings.json`, which Rust writes from
 * its own serializer. The wording is `turnEnding.test.ts`, the fold `reducer.test.ts`.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello } from "./types";
/** What Rust really sends for each way a turn can end: its events payload, and the snapshot payload after folding
 *  the same events (a panel reload, a tab switch back). */
import fixture from "./fixtures/turn-endings.json";

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

const rowText = (c: HTMLElement) => Array.from(c.querySelectorAll(".row-ending .row-body")).map((el) => el.textContent);
const bandEnding = (c: HTMLElement) => c.querySelector(".status-band .band-ending")?.textContent ?? null;

/** What each case's row and band say. `cli_crash_closed` is one failed row (the turn was no longer active when the
 *  session closed) and `stream_lost` one lost row. */
const EXPECTED: Record<string, { row: string; band: string }> = {
  api_error: {
    row: 'the turn ended with an error (HTTP 529): API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}',
    band: "turn failed",
  },
  rate_limited: { row: "rate or usage limit reached (HTTP 429): API Error: 429 rate_limit_error", band: "rate limited" },
  context_full: { row: "stopped: the context is full: Prompt is too long", band: "context full" },
  max_turns: { row: "stopped at the turn limit: Reached maximum number of turns (3)", band: "turn limit" },
  hook_stopped: { row: "stopped by a hook", band: "stopped by a hook" },
  interrupt: { row: "interrupted", band: "interrupted" },
  cli_crash_closed: { row: "the turn ended with an error", band: "turn failed" },
  stream_lost: { row: "the turn did not finish: the session ended", band: "turn did not finish" },
};

describe("a turn that did not complete", () => {
  it("the fixture holds every case this file expects, and no other", () => {
    expect(fixture.cases.map((c) => c.name).sort()).toEqual(Object.keys(EXPECTED).sort());
  });

  describe.each(fixture.cases)("$name", (c) => {
    it("arriving as events draws the row after the partial reply, and the band says it", () => {
      const { container } = started();
      dispatch({ ...c.events, tab: 1 });
      expect(rowText(container)).toEqual([EXPECTED[c.name].row]);
      expect(bandEnding(container)).toBe(EXPECTED[c.name].band);
      // The first three rows are the conversation (a session that ended adds its own banner row after them).
      const kinds = Array.from(container.querySelectorAll(".row")).slice(0, 3).map((r) => r.getAttribute("data-sign"));
      expect(kinds).toEqual(["›", "", "·"]);
      expect(container.querySelector(".row-ending")!.previousElementSibling!.textContent).toContain("Looking at the parser");
    });

    it("arriving as a snapshot (a reload, a tab switch back) draws the same row and the same band", () => {
      const { container } = started();
      dispatch({ ...c.snapshot, tab: 1 });
      expect(rowText(container)).toEqual([EXPECTED[c.name].row]);
      expect(bandEnding(container)).toBe(EXPECTED[c.name].band);
    });
  });

  it("the band stops saying it when the next turn starts, and the row stays", () => {
    const { container } = started();
    const failed = fixture.cases.find((c) => c.name === "api_error")!;
    dispatch({ ...failed.events, tab: 1 });
    expect(bandEnding(container)).toBe("turn failed");
    const next = [{ type: "turn_started", turn_id: "t2" }];
    dispatch({ kind: "events", tab: 1, fromRevision: failed.events.throughRevision, throughRevision: failed.events.throughRevision + 1, events: next });
    expect(bandEnding(container)).toBeNull();
    expect(rowText(container)).toHaveLength(1);
  });

  it("a completed turn leaves neither a row nor a band word", () => {
    const { container } = started();
    const failed = fixture.cases.find((c) => c.name === "api_error")!;
    const events = failed.events.events.map((e) => (e.type === "turn_completed" ? { ...e, outcome: "completed", detail: { reason: null, api_error_status: null, message: null } } : e));
    dispatch({ ...failed.events, tab: 1, events });
    expect(rowText(container)).toEqual([]);
    expect(bandEnding(container)).toBeNull();
  });
});
