// @vitest-environment jsdom
/**
 * The model and the effort of a running session, changed directly: the picker opens from the session's own list, also
 * during a turn, with no turn started and nothing queued; a choice goes straight to the session and the band says when
 * it took effect; a sidecar without the capability keeps the slash commands.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello, ModelOption } from "./types";
import { binding, TABLE } from "./testFixtures";
import type { PanelTable } from "./keymap";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;
let widen: (container: HTMLElement) => void;

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

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB = {
  id: 1, number: 1, label: "1", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null,
} as const;
const PANEL: PanelTable = {
  ...TABLE,
  bindings: [...TABLE.bindings, binding(["<leader>", "M"], "model.pick", "model"), binding(["<leader>", "E"], "effort.pick", "effort")],
};

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

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));

const row = (value: string, displayName: string, over: Partial<ModelOption> = {}): ModelOption => ({
  value, resolvedModel: `claude-${value}-5-5`, displayName, description: "", supportsEffort: true,
  supportedEffortLevels: ["low", "medium", "max"], supportsAutoMode: true, ...over,
});
const MODELS = [row("default", "Default", { resolvedModel: "claude-opus-5-5" }), row("opus", "Opus"), row("sonnet", "Sonnet")];
const APPLIED = { model: "claude-opus-5-5", effort: "medium" };

function live(overrides: Partial<AgentUiState> = {}, direct = true, agent?: string) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [agent === undefined ? TAB : { ...TAB, agent }] });
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 0,
    state: {
      ...initialState(),
      status: { kind: "running" },
      backend: "sidecar",
      model: "claude-opus-5-5",
      effort: "medium",
      capabilities: { ...initialState().capabilities, directSettings: direct },
      ...overrides,
    },
  });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: PANEL, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const ofType = (type: string) => posted.filter((m) => m.type === type);
const last = (type: string) => ofType(type).at(-1)!;
const answerList = (models = MODELS, applied = APPLIED) =>
  dispatch({ kind: "models_result", requestId: last("list_models").request_id, tab: 1, models, applied });
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const bandMessage = (c: HTMLElement) => c.querySelector(".band-message")?.textContent;
const picker = (c: HTMLElement) => c.querySelector(".slash-picker");
const rows = (c: HTMLElement) => [...c.querySelectorAll(".slash-picker-name")].map((el) => el.textContent);
const choose = (c: HTMLElement, name: string) =>
  fireEvent.click([...c.querySelectorAll(".slash-picker-row")].find((el) => el.textContent?.includes(name))!);
function press(c: HTMLElement, key: string) {
  fireEvent.keyDown(root(c), { key: " " });
  fireEvent.keyDown(root(c), { key, shiftKey: true });
}

describe("the picker, on a session that changes directly", () => {
  it("opens during a turn from the session's list: nothing goes to the CLI as a message and nothing is queued", () => {
    const { container } = live({ activeTurnId: "t1" });
    press(container, "M");
    expect(ofType("list_models")).toHaveLength(1);
    expect(ofType("send_message")).toEqual([]);
    answerList();
    expect(rows(container)).toEqual(["Default", "Opus", "Sonnet"]);
    expect(container.querySelector(".slash-picker-current-marker")?.closest("li")?.textContent).toContain("Opus");
    expect(ofType("send_message")).toEqual([]);
  });

  it("a click on the band opens it too, also during a turn", () => {
    const { container } = live({ activeTurnId: "t1" });
    fireEvent.click(container.querySelector("button.band-effort")!);
    expect(ofType("list_models")).toHaveLength(1);
    answerList();
    expect(rows(container)).toEqual(["low", "medium", "max", "as the settings say"]);
  });

  it("a typed bare /model opens it, in a turn and out of one, and neither sends nor queues anything", () => {
    for (const activeTurnId of ["t1", null]) {
      posted.length = 0;
      const { container, unmount } = live({ activeTurnId });
      fireEvent.keyDown(root(container), { key: "i" });
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "/model" } });
      fireEvent.keyDown(box, { key: "Enter" });
      expect(ofType("list_models")).toHaveLength(1);
      expect(ofType("send_message")).toEqual([]);
      expect(ofType("queue_message")).toEqual([]);
      unmount();
    }
  });

  function typeIn(container: HTMLElement, text: string, key: Record<string, unknown> = { key: "Enter" }) {
    fireEvent.keyDown(root(container), { key: "i" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: text } });
    fireEvent.keyDown(box, key);
  }

  it("a typed /model <name> during a turn changes it at once, queues nothing, and the band says when it applies", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/model sonnet");
    expect(last("set_model").model).toBe("sonnet");
    expect(ofType("queue_message")).toEqual([]);
    expect(ofType("send_message")).toEqual([]);
    expect(ofType("history_push").map((m) => m.text)).toEqual(["/model sonnet"]);
    expect(container.querySelector("textarea")!.value).toBe("");
  });

  it("Ctrl+Enter on /model <name> changes it too, and the turn goes on", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/model sonnet", { key: "Enter", ctrlKey: true });
    expect(last("set_model").model).toBe("sonnet");
    expect(ofType("send_now")).toEqual([]);
  });

  it("Ctrl+x Enter on a typed setting during a turn changes nothing now: it is held for the turn's end", () => {
    for (const text of ["/model sonnet", "/effort low"]) {
      posted.length = 0;
      const { container, unmount } = live({ activeTurnId: "t1" });
      fireEvent.keyDown(root(container), { key: "i" });
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: text } });
      box.setSelectionRange(text.length, text.length);
      fireEvent.keyDown(box, { key: "x", ctrlKey: true });
      fireEvent.keyDown(box, { key: "Enter" });
      expect(ofType("set_model"), text).toEqual([]);
      expect(ofType("list_models"), text).toEqual([]);
      expect(ofType("history_push"), text).toEqual([]);
      expect(ofType("queue_message"), text).toEqual([expect.objectContaining({ text, after_turn: true })]);
      unmount();
    }
  });

  it("between turns /model <name> stays a message", () => {
    const { container } = live({ activeTurnId: null });
    typeIn(container, "/model sonnet");
    expect(ofType("set_model")).toEqual([]);
    expect(ofType("send_message").map((m) => m.text)).toEqual(["/model sonnet"]);
  });

  it.each(["t1", null])("ACP Ctrl+x Enter applies typed settings immediately with active turn %s", (activeTurnId) => {
    for (const text of ["/model sonnet", "/effort low"]) {
      posted.length = 0;
      const { container, unmount } = live({ activeTurnId }, false, "acp:gem");
      fireEvent.keyDown(root(container), { key: "i" });
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: text } });
      box.setSelectionRange(text.length, text.length);
      fireEvent.keyDown(box, { key: "x", ctrlKey: true });
      fireEvent.keyDown(box, { key: "Enter" });
      if (text.startsWith("/effort")) {
        expect(ofType("list_models")).toHaveLength(1);
        dispatch({ kind: "models_result", requestId: last("list_models").request_id, tab: 1,
          models: MODELS, applied: APPLIED, efforts: [{ value: "low", displayName: "Low", description: "" }] });
        expect(last("apply_effort")).toMatchObject({ tab: 1, effort: "low" });
      } else {
        expect(last("set_model")).toMatchObject({ tab: 1, model: "sonnet" });
      }
      expect(ofType("queue_message")).toEqual([]);
      expect(ofType("send_message")).toEqual([]);
      unmount();
    }
  });

  it("a typed /effort <level> during a turn checks the session's list and changes it at once", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/effort low");
    expect(ofType("queue_message")).toEqual([]);
    answerList();
    expect(last("apply_effort").effort).toBe("low");
    expect(ofType("queue_message")).toEqual([]);
  });

  it("/effort auto is the settings row", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/effort auto");
    answerList();
    expect(last("apply_effort").effort).toBe("");
  });

  it("a level the session does not offer is queued, for the CLI to answer at the turn's end", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/effort lowest");
    answerList();
    expect(ofType("apply_effort")).toEqual([]);
    expect(ofType("queue_message").map((m) => m.text)).toEqual(["/effort lowest"]);
  });

  it("a list that does not answer leaves a typed level queued", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/effort low");
    dispatch({ kind: "command_result", requestId: last("list_models").request_id, ok: false, error: "no" });
    expect(ofType("queue_message").map((m) => m.text)).toEqual(["/effort low"]);
  });

  it("with no direct route, and on another agent's tab, a typed setting is queued as before", () => {
    for (const [direct, agent] of [[false, undefined], [true, "codex"]] as const) {
      posted.length = 0;
      const { container, unmount } = live({ activeTurnId: "t1" }, direct, agent);
      typeIn(container, "/model sonnet");
      expect(ofType("set_model")).toEqual([]);
      expect(ofType("queue_message").map((m) => m.text)).toEqual(["/model sonnet"]);
      unmount();
    }
  });

  it("a command with more than one word after it is queued", () => {
    const { container } = live({ activeTurnId: "t1" });
    typeIn(container, "/model sonnet please");
    expect(ofType("set_model")).toEqual([]);
    expect(ofType("queue_message").map((m) => m.text)).toEqual(["/model sonnet please"]);
  });

  it("Ctrl+Enter on a bare /model opens it too, and the turn goes on", () => {
    const { container } = live({ activeTurnId: "t1" });
    fireEvent.keyDown(root(container), { key: "i" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "/model" } });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(ofType("list_models")).toHaveLength(1);
    expect(ofType("send_now")).toEqual([]);
  });

  it("a typed /model with an argument stays a message", () => {
    const { container } = live();
    fireEvent.keyDown(root(container), { key: "i" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "/model sonnet" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(last("send_message").text).toBe("/model sonnet");
    expect(ofType("set_model")).toEqual([]);
  });

  it("a list for a tab no longer shown opens nothing", () => {
    const { container } = live();
    press(container, "M");
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, { ...TAB, id: 2, number: 2 }] });
    answerList();
    expect(picker(container)).toBeNull();
  });

  it("a list that finds another overlay open says so instead of nothing", () => {
    const { container } = live();
    press(container, "M");
    fireEvent.keyDown(root(container), { key: "?" });
    answerList();
    expect(picker(container)).toBeNull();
    expect(bandMessage(container)).toBe("close what is open to choose the model");
  });

  it("a refused list says why, as text", () => {
    const { container } = live();
    press(container, "M");
    dispatch({ kind: "command_result", requestId: last("list_models").request_id, ok: false, error: "no\u202e list" });
    expect(bandMessage(container)).toBe("no⟨U+202E⟩ list");
    expect(picker(container)).toBeNull();
  });

  it("a model that takes no level says so instead of opening an empty effort picker", () => {
    const { container } = live();
    press(container, "E");
    answerList([row("old", "Old", { resolvedModel: "claude-old", supportsEffort: false, supportedEffortLevels: [] })], { model: "claude-old", effort: "" });
    expect(picker(container)).toBeNull();
    expect(bandMessage(container)).toBe("this model takes no effort level");
  });
});

describe("a choice", () => {
  it("goes straight to the session during a turn, and the band says the running response finishes on the old model", () => {
    const { container } = live({ activeTurnId: "t1" });
    press(container, "M");
    answerList();
    choose(container, "Sonnet");
    expect(last("set_model")).toMatchObject({ tab: 1, model: "sonnet" });
    expect(ofType("send_message")).toEqual([]);
    expect(picker(container)).toBeNull();
    dispatch({ kind: "settings_result", requestId: last("set_model").request_id, tab: 1, applied: { model: "claude-sonnet-5-5", effort: "medium" } });
    expect(bandMessage(container)).toBe("model: sonnet-5-5; the running response finishes on opus-5-5");
  });

  it("says the next turn when no turn runs", () => {
    const { container } = live();
    press(container, "M");
    answerList();
    choose(container, "Sonnet");
    dispatch({ kind: "settings_result", requestId: last("set_model").request_id, tab: 1, applied: { model: "claude-sonnet-5-5", effort: "medium" } });
    expect(bandMessage(container)).toBe("model: sonnet-5-5, from the next turn");
  });

  it("the band shows the model the session reports", () => {
    const { container } = live();
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events: [{ type: "model_reported", model: "claude-sonnet-5-5" }] });
    expect(container.querySelector("button.band-model")!.textContent).toBe("sonnet-5-5");
  });

  it("an effort goes to the session, and the settings row clears it", () => {
    const { container } = live({ activeTurnId: "t1" });
    press(container, "E");
    answerList();
    choose(container, "low");
    expect(last("apply_effort")).toMatchObject({ tab: 1, effort: "low" });
    dispatch({ kind: "settings_result", requestId: last("apply_effort").request_id, tab: 1, applied: { model: "claude-opus-5-5", effort: "low" } });
    expect(bandMessage(container)).toBe("effort: low, from the next request");
    wait(TYPING_GUARD_MS + 50);
    press(container, "E");
    answerList();
    choose(container, "as the settings say");
    expect(last("apply_effort").effort).toBe("");
    expect(ofType("send_message")).toEqual([]);
  });

  it("a refusal says the CLI's words and the session keeps its model", () => {
    const { container } = live();
    press(container, "M");
    answerList();
    choose(container, "Sonnet");
    dispatch({ kind: "command_result", requestId: last("set_model").request_id, ok: false, error: "Model 'x' not found" });
    expect(bandMessage(container)).toBe("Model 'x' not found");
    expect(container.querySelector("button.band-model")!.textContent).toBe("opus-5-5");
  });
});

describe("a sidecar without the capability", () => {
  it("keeps the slash commands and says nothing new", () => {
    const { container } = live({}, false);
    press(container, "M");
    expect(last("send_message").text).toBe("/model");
    expect(ofType("list_models")).toEqual([]);
  });

  it("during a turn the key says what the band says", () => {
    const { container } = live({ activeTurnId: "t1" }, false);
    press(container, "M");
    expect(ofType("send_message")).toEqual([]);
    expect(ofType("list_models")).toEqual([]);
    expect(bandMessage(container)).toBe("a turn is running: choose the model when it ends");
  });

  it("a typed bare /model stays a message", () => {
    const { container } = live({}, false);
    fireEvent.keyDown(root(container), { key: "i" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "/model" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(last("send_message").text).toBe("/model");
  });
});

it("another agent's tab asks for no list, whatever its capabilities say", () => {
  const { container } = live({}, true, "codex");
  press(container, "M");
  expect(ofType("list_models")).toEqual([]);
  expect(bandMessage(container)).toBe("Choosing the model of a codex session is not available");
});
