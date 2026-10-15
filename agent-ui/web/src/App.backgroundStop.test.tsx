// @vitest-environment jsdom
/**
 * The commands a Codex tab's agent left running after its turns ended: the band counts them while there are
 * any, `<leader>k` asks before it stops them (they stop with the session), and only a plain, unrepeated `y`
 * given a moment after the question sends the stop, naming the tab and the count it asked about.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello } from "./types";
import { binding, TABLE } from "./testFixtures";
import type { PanelTable } from "./keymap";

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
  // `performance` too: the question's guard reads the clock through nothing else.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  widen = stubBandWidth();
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
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 codex", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "codex",
} as const;
const TAB2 = { ...TAB1, id: 2, number: 2, label: "2 codex" } as const;

const PANEL: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "k"], "tab.stop-background", "stop background commands")] };

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

/** Tab 1 live with `background` commands left running, the pane focused and the keys arrived, in BROWSE. */
function started(background: number, overrides: Partial<AgentUiState> = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1, TAB2] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState({ backgroundCalls: background, ...overrides }) });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: PANEL, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const bandText = (c: HTMLElement) => c.querySelector(".status-band")!.textContent ?? "";
const stops = () => posted.filter((m) => m.type === "stop_background");

/** `<leader>k`, then the question's own on-screen wait. */
function ask(container: HTMLElement) {
  fireEvent.keyDown(root(container), { key: " " });
  fireEvent.keyDown(root(container), { key: "k" });
  wait(TYPING_GUARD_MS + 50);
}

describe("commands left running in the background", () => {
  it("are counted on the band only while there are any", () => {
    const { container } = started(0);
    expect(container.querySelector(".band-background")).toBeNull();
    dispatch({ kind: "events", tab: 1, throughRevision: 1, events: [{ type: "background_calls", running: 1 }] });
    expect(container.querySelector(".band-background")!.textContent).toBe("1 running in background");
    dispatch({ kind: "events", tab: 1, throughRevision: 2, events: [{ type: "background_calls", running: 2 }] });
    expect(container.querySelector(".band-background")!.textContent).toBe("2 running in background");
    dispatch({ kind: "events", tab: 1, throughRevision: 3, events: [{ type: "background_calls", running: 0 }] });
    expect(container.querySelector(".band-background")).toBeNull();
  });

  it("the key says so when nothing runs, or when a turn does, and sends nothing", () => {
    const { container } = started(0);
    ask(container);
    expect(container.querySelector(".band-message")!.textContent).toBe("nothing is running in the background");
    cleanup();
    const again = started(2, { activeTurnId: "turn-1" });
    ask(again.container);
    expect(again.container.querySelector(".band-message")!.textContent).toContain("interrupting the turn stops them too");
    expect(stops()).toEqual([]);
  });

  it("asks first, and y sends the stop naming its tab and the count it asked about", () => {
    const { container } = started(2);
    ask(container);
    expect(bandText(container)).toContain(
      "Stop the 2 commands Codex left running? This ends the session; the conversation can be resumed. y/n",
    );
    expect(stops()).toEqual([]);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([expect.objectContaining({ type: "stop_background", tab: 1, running: 2 })]);
    expect(bandText(container)).not.toContain("left running?");
  });

  it("names one command in the singular", () => {
    const { container } = started(1);
    ask(container);
    expect(bandText(container)).toContain("Stop the command Codex left running? This ends the session");
  });

  it("n and Escape put it away and send nothing", () => {
    for (const key of ["n", "Escape"]) {
      const { container } = started(2);
      ask(container);
      fireEvent.keyDown(root(container), { key });
      expect(bandText(container)).not.toContain("left running?");
      fireEvent.keyDown(root(container), { key: "y" });
      expect(stops()).toEqual([]);
      cleanup();
    }
  });

  it("a y too soon after the question, held, or with a modifier does not answer it", () => {
    let { container } = started(2);
    fireEvent.keyDown(root(container), { key: " " });
    fireEvent.keyDown(root(container), { key: "k" });
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([]);
    cleanup();

    ({ container } = started(2));
    ask(container);
    fireEvent.keyDown(root(container), { key: "y", repeat: true });
    expect(stops()).toEqual([]);
    cleanup();

    ({ container } = started(2));
    ask(container);
    fireEvent.keyDown(root(container), { key: "y", ctrlKey: true });
    expect(stops()).toEqual([]);
  });

  it("asks again, and sends nothing, when the count moved while the question was up", () => {
    const { container } = started(2);
    ask(container);
    dispatch({ kind: "events", tab: 1, throughRevision: 1, events: [{ type: "background_calls", running: 3 }] });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([]);
    expect(bandText(container)).toContain("Stop the 3 commands Codex left running?");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([expect.objectContaining({ tab: 1, running: 3 })]);
  });

  it("a switch to another tab ends the question", () => {
    const { container } = started(2);
    ask(container);
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
    wait(TYPING_GUARD_MS + 50);
    expect(bandText(container)).not.toContain("left running?");
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([]);
  });

  it("the ? overlay lists the key", () => {
    const { container } = started(0);
    fireEvent.keyDown(root(container), { key: "?", shiftKey: true });
    const overlay = container.querySelector(".keymap-overlay")!;
    expect(overlay.textContent).toContain("Space k");
    expect(overlay.textContent).toContain("stop background commands");
  });
});
