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
import type { AgentUiState, BackgroundTaskRecord, Hello } from "./types";
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
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 codex", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "codex",
} as const;
const TAB2 = { ...TAB1, id: 2, number: 2, label: "2 codex" } as const;

const PANEL: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "k"], "tab.stop-background", "stop background work")] };

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
function started(background: number, overrides: Partial<AgentUiState> = {}, agent = "codex") {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB1, agent }, TAB2] });
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

const taskRow = (id: string, kind: BackgroundTaskRecord["kind"] = "shell"): BackgroundTaskRecord => ({
  task_id: id, run_id: "1", kind, task_type: kind, description: id, ambient: false, subagent_type: null,
  parent_task_id: null, tool_use_id: null, workflow_name: null, status: "running", live: true,
  started_at: Date.now(), ended_at: null, finished_seq: null, summary: null, last_tool_name: null, usage: null,
  error: null, is_backgrounded: true, reason: null, output_file: null, workflow_progress: null,
});
function claudeState(rows = [taskRow("one"), taskRow("two"), taskRow("monitor", "monitor")]): Partial<AgentUiState> {
  return { backend: "sidecar", backgroundTasks: { rows, query_start: false }, capabilities: {
    ...initialState().capabilities, backgroundTasks: true, taskControl: true, taskOutput: true, workflowProgress: true,
  } };
}

/** `<leader>k`, then the question's own on-screen wait. */
function ask(container: HTMLElement) {
  fireEvent.keyDown(root(container), { key: " " });
  fireEvent.keyDown(root(container), { key: "k" });
  wait(TYPING_GUARD_MS + 50);
}

describe("commands left running in the background", () => {
  it("Claude asks in task nouns, excludes system tasks and keeps the session", () => {
    const rows = [taskRow("one"), taskRow("two"), taskRow("monitor", "monitor"), { ...taskRow("system"), ambient: true }];
    const { container } = started(3, claudeState(rows), "claude");
    ask(container);
    expect(bandText(container)).toContain("Stop 2 shells, 1 monitor? y/n");
    expect(bandText(container)).not.toContain("ends the session");
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([expect.objectContaining({ tab: 1, running: 3 })]);
  });

  it("Claude asks again if the same count now names different kinds", () => {
    const { container } = started(3, claudeState(), "claude");
    ask(container);
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ backgroundCalls: 3,
      ...claudeState([taskRow("one"), taskRow("two"), taskRow("agent", "local_agent")]) }) });
    fireEvent.keyDown(root(container), { key: "y" });
    expect(stops()).toEqual([]);
    expect(bandText(container)).toContain("Stop 2 shells, 1 local agent? y/n");
  });

  it("Claude without task control cannot ask a misleading stop-all question", () => {
    const overrides = claudeState(); overrides.capabilities!.taskControl = false;
    const { container } = started(3, overrides, "claude");
    ask(container);
    expect(bandText(container)).toContain("this sidecar cannot stop one task");
    expect(stops()).toEqual([]);
  });

  it("the pill and tasks-line Enter open the overlay and capture every key", () => {
    const { container } = started(3, claudeState(), "claude");
    fireEvent.click(container.querySelector(".band-background")!);
    expect(container.querySelector(".tasks-overlay")).not.toBeNull();
    fireEvent.keyDown(root(container), { key: "j" });
    expect(container.querySelector('.tasks-overlay [aria-selected="true"]')!.textContent).toContain("two");
    fireEvent.keyDown(root(container), { key: "x" });
    expect(posted.filter((p) => p.type === "stop_task")).toEqual([expect.objectContaining({ tab: 1, task_id: "two" })]);
    fireEvent.keyDown(root(container), { key: "q" });
    const line = container.querySelector<HTMLButtonElement>(".tasks-line button")!;
    act(() => line.focus());
    fireEvent.keyDown(line, { key: "Enter" });
    // jsdom does not synthesize a button's native click on Enter.
    fireEvent.click(line);
    expect(container.querySelector(".tasks-overlay")).not.toBeNull();
    fireEvent.keyDown(root(container), { key: "i" });
    expect(container.querySelector('[data-panel-mode="input"]')).toBeNull();
  });

  it("Enter on a background card opens that task and o returns through the read-only scratch route", () => {
    const row = { ...taskRow("build"), tool_use_id: "call" };
    const { container } = started(1, { ...claudeState([row]), toolCalls: [{ seq: 0, toolUseId: "call", name: "Bash",
      input: { command: "make", run_in_background: true }, result: { content: "launched", isError: false } }], nextSeq: 1 }, "claude");
    fireEvent.keyDown(root(container), { key: "Enter" });
    expect(container.querySelector(".tasks-detail h3")!.textContent).toContain("build");
    fireEvent.keyDown(root(container), { key: "o" });
    const read = posted.find((p) => p.type === "task_output")!;
    dispatch({ kind: "task_output_result", requestId: read.request_id, tab: 1, taskId: "build",
      output: "\x1b[31moutput\x1b[0m\u202e", totalBytes: 24, truncated: false });
    expect(posted.find((p) => p.type === "view_in_editor")).toEqual(expect.objectContaining({ title: "Task output", text: "output⟨U+202E⟩" }));
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(container.querySelector(".tasks-detail h3")!.textContent).toContain("build");
    dispatch({ kind: "events", tab: 1, throughRevision: 1, events: [{ type: "cli_cycle_started", turn_id: "wake", cause: null }] });
    expect(container.querySelector(".tasks-overlay")!.textContent).toContain("1 update below");
  });

  it("tab switches close the overlay and discard delayed output", () => {
    const { container } = started(3, claudeState(), "claude");
    fireEvent.click(container.querySelector(".band-background")!);
    fireEvent.keyDown(root(container), { key: "o" });
    const read = posted.find((p) => p.type === "task_output")!;
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "task_output_result", requestId: read.request_id, tab: 1, taskId: "one", output: "old output", totalBytes: 10, truncated: false });
    expect(container.querySelector(".tasks-overlay")).toBeNull();
    expect(posted.some((p) => p.type === "view_in_editor")).toBe(false);
  });

  it("on a tab without a task roster, the count is band text that opens the tab details, not the tasks list", () => {
    const { container } = started(2);
    const segment = container.querySelector(".band-background")!;
    expect(segment.tagName).toBe("SPAN");
    expect(segment.closest(".band-open")).not.toBeNull();
    fireEvent.click(segment);
    expect(container.querySelector(".tasks-overlay")).toBeNull();
  });

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

  it("the key on a Claude tab says its Claude Code does not report background work, and sends nothing", () => {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB1, label: "1 claude", agent: "claude" }, TAB2] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: PANEL, newTabChord: "Ctrl+b c" });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    wait(TYPING_GUARD_MS + 50);
    ask(rendered.container);
    expect(rendered.container.querySelector(".band-message")!.textContent).toBe(
      "this Claude Code sidecar does not report background work",
    );
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
    expect(overlay.textContent).toContain("stop background work");
  });
});
