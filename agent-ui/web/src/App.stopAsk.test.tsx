// @vitest-environment jsdom
/**
 * A tab brought forward to be stopped (`arrive` with `intent: "stop"`) asks in its own chat, in the band where the
 * other y/n prompts are drawn: `Stop this turn? (y/n)` while a turn runs, else the stop-background question. Only a
 * plain, unrepeated `y` given past the y/n guard acts, and it sends the message the chat's own stop keys send
 * (`interrupt`, `stop_background`) for the tab it asked about. The arrival itself counts as a key, so a `y` typed on
 * through it is refused.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, BackgroundTaskRecord, Hello } from "./types";
import { TABLE } from "./testFixtures";

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
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  widen = stubBandWidth();
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** The band draws its segments only once it knows its width; jsdom measures nothing. */
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

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 codex", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "codex",
} as const;
const TAB2 = { ...TAB1, id: 2, number: 2, label: "2 codex" } as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

const taskRow = (id: string): BackgroundTaskRecord => ({
  task_id: id, run_id: "1", kind: "shell", task_type: "shell", description: id, ambient: false, subagent_type: null,
  parent_task_id: null, tool_use_id: null, workflow_name: null, status: "running", live: true,
  started_at: Date.now(), ended_at: null, finished_seq: null, summary: null, last_tool_name: null, usage: null,
  error: null, is_backgrounded: true, reason: null, output_file: null, workflow_progress: null,
});

/** Tab 1 live with `overrides`, settled in BROWSE long past any key, then brought forward to be stopped. */
function arrivedToStop(overrides: Partial<AgentUiState>, agent = "codex") {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB1, agent }, TAB2] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState(overrides) });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(1000);
  dispatch({ kind: "arrive", intent: "stop" });
  return rendered;
}

const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const bandText = (c: HTMLElement) => c.querySelector(".status-band")!.textContent ?? "";
const interrupts = () => posted.filter((m) => m.type === "interrupt");
const stops = () => posted.filter((m) => m.type === "stop_background");
const TURN = { activeTurnId: "turn-1" };

describe("stop, asked in the chat", () => {
  it("stop_intent_shows_stop_this_turn_when_a_turn_runs", () => {
    const { container } = arrivedToStop({ ...TURN, backgroundCalls: 2 });
    expect(bandText(container)).toContain("Stop this turn? (y/n)");
    expect(interrupts()).toEqual([]);
  });

  it("stop_intent_shows_stop_background_when_only_background_runs: Codex", () => {
    const { container } = arrivedToStop({ backgroundCalls: 2 });
    expect(bandText(container)).toContain("Stop the 2 commands Codex left running?");
    expect(bandText(container)).not.toContain("Stop this turn?");
  });

  it("stop_intent_shows_stop_background_when_only_background_runs: Claude", () => {
    const { container } = arrivedToStop(
      {
        backend: "sidecar",
        backgroundTasks: { rows: [taskRow("one"), taskRow("two")], query_start: false },
        capabilities: { ...initialState().capabilities, backgroundTasks: true, taskControl: true },
      },
      "claude",
    );
    expect(bandText(container)).toContain("Stop 2 shells? y/n");
  });

  it("Claude Code working where the chat cannot show it is asked about as a turn, and y interrupts it", () => {
    const { container } = arrivedToStop({ backend: "sidecar", cliUnseenBusy: true }, "claude");
    expect(bandText(container)).toContain("Stop this turn? (y/n)");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([{ type: "interrupt", request_id: expect.any(String), tab: 1 }]);
  });

  it("a y after the unseen work ended stops nothing", () => {
    const { container } = arrivedToStop({ backend: "sidecar", cliUnseenBusy: true }, "claude");
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ backend: "sidecar" }) });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
  });

  it("a stop arrival on a tab with nothing running asks nothing and sends nothing", () => {
    const { container } = arrivedToStop({});
    expect(bandText(container)).not.toContain("(y/n)");
    expect(bandText(container)).not.toContain("? y/n");
    expect(bandText(container)).toContain("nothing is running in this tab");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
    expect(stops()).toEqual([]);
  });

  it("an arrival to show, or with no intent, asks nothing", () => {
    for (const intent of ["show", undefined]) {
      const rendered = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "tabs", active: 1, tabs: [TAB1, TAB2] });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState(TURN) });
      dispatch({ kind: "pane_focus", focused: true });
      dispatch(intent === undefined ? { kind: "arrive" } : { kind: "arrive", intent });
      expect(bandText(rendered.container)).not.toContain("Stop this turn?");
      cleanup();
    }
  });

  it("y_inside_the_yn_guard_is_refused: a y typed on through the arrival stops nothing", () => {
    const { container } = arrivedToStop(TURN);
    wait(100);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
    expect(bandText(container)).not.toContain("Stop this turn?");
    expect(bandText(container)).toContain("nothing was stopped");
  });

  it("y_after_the_guard_interrupts: the message is the chat's own interrupt, for that tab", () => {
    const { container } = arrivedToStop(TURN);
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([{ type: "interrupt", request_id: expect.any(String), tab: 1 }]);
    expect(bandText(container)).not.toContain("Stop this turn?");
    expect(stops()).toEqual([]);
  });

  it("y_after_the_guard_interrupts: a repeated or modified y does not", () => {
    for (const extra of [{ repeat: true }, { ctrlKey: true }]) {
      const { container } = arrivedToStop(TURN);
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(root(container), { key: "y", ...extra });
      expect(interrupts()).toEqual([]);
      cleanup();
    }
  });

  it("a y after the turn ended stops nothing", () => {
    const { container } = arrivedToStop(TURN);
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
  });

  it("a y after the turn ended and another began stops nothing: the question was about the first", () => {
    const { container } = arrivedToStop(TURN);
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state: snapshotState({ activeTurnId: "turn-2" }) });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
    expect(bandText(container)).toContain("nothing was stopped");
  });

  it("a y after one snapshot swapped the turn stops nothing", () => {
    const { container } = arrivedToStop(TURN);
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ activeTurnId: "turn-2" }) });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
  });

  it("y_on_background_stops_background: the stop names the tab and the count it asked about", () => {
    const { container } = arrivedToStop({ backgroundCalls: 2 });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([expect.objectContaining({ type: "stop_background", tab: 1, running: 2 })]);
    expect(interrupts()).toEqual([]);
  });

  it("n_and_esc_do_nothing: on either question", () => {
    for (const overrides of [TURN, { backgroundCalls: 2 }]) {
      for (const key of ["n", "Escape"]) {
        const { container } = arrivedToStop(overrides);
        wait(TYPING_GUARD_MS + 50);
        fireEvent.keyDown(root(container), { key });
        expect(bandText(container)).not.toContain("Stop this turn?");
        expect(bandText(container)).not.toContain("left running?");
        wait(TYPING_GUARD_MS + 50);
        fireEvent.keyDown(root(container), { key: "y" });
        expect(interrupts()).toEqual([]);
        expect(stops()).toEqual([]);
        cleanup();
      }
    }
  });

  it("a switch to another tab ends the question", () => {
    const { container } = arrivedToStop(TURN);
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    expect(bandText(container)).not.toContain("Stop this turn?");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(interrupts()).toEqual([]);
  });
});
