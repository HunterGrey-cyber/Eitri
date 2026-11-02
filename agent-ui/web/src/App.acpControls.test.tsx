// @vitest-environment jsdom
/**
 * The model and the effort of an ACP agent's tab, chosen from the options the agent itself lists: the same keys as a
 * Claude tab (`<leader>M`, `<leader>E`, a typed `/model` and `/effort`, a click on the band), the agent's own words
 * in the rows, and a plain sentence when the agent lists nothing.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, EffortChoice, Hello, ModelOption } from "./types";
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
  permissionModes: ["auto", "bypass"],
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

const model = (value: string, displayName: string): ModelOption => ({
  value, resolvedModel: "", displayName, description: "", supportsEffort: false, supportedEffortLevels: [], supportsAutoMode: false,
});
const MODELS = [model("gpt-6-astra", "6 Astra"), model("gpt-6-sol", "6 Sol")];
const EFFORTS: EffortChoice[] = [
  { value: "low", displayName: "Low", description: "" },
  { value: "high", displayName: "High", description: "slow" },
];
const APPLIED = { model: "gpt-6-astra", effort: "high" };

function live(overrides: Partial<AgentUiState> = {}, agent = "acp:gem") {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, agent }] });
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 0,
    state: {
      ...initialState(),
      status: { kind: "running" },
      backend: "sidecar",
      model: "gpt-6-astra",
      capabilities: { ...initialState().capabilities, directSettings: false },
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
const answerList = (models: ModelOption[] = MODELS, efforts: EffortChoice[] = EFFORTS, applied = APPLIED) =>
  dispatch({ kind: "models_result", requestId: last("list_models").request_id, tab: 1, models, applied, efforts });
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const flash = (c: HTMLElement) => c.querySelector(".band-message")?.textContent;
const picker = (c: HTMLElement) => c.querySelector(".slash-picker");
const rows = (c: HTMLElement) => [...c.querySelectorAll(".slash-picker-name")].map((el) => el.textContent);
const choose = (c: HTMLElement, name: string) =>
  fireEvent.click([...c.querySelectorAll(".slash-picker-row")].find((el) => el.textContent?.includes(name))!);
function press(c: HTMLElement, key: string) {
  fireEvent.keyDown(root(c), { key: " " });
  fireEvent.keyDown(root(c), { key, shiftKey: true });
}
function typeIn(container: HTMLElement, text: string) {
  fireEvent.keyDown(root(container), { key: "i" });
  const box = container.querySelector("textarea")!;
  fireEvent.change(box, { target: { value: text } });
  fireEvent.keyDown(box, { key: "Enter" });
}

describe("an ACP agent's tab chooses its model and effort from the agent's own list", () => {
  it("<leader>M asks the session for its list with no direct capability, and shows the agent's words", () => {
    const { container } = live();
    press(container, "M");
    expect(ofType("list_models")).toHaveLength(1);
    expect(ofType("send_message")).toEqual([]);
    answerList();
    expect(rows(container)).toEqual(["6 Astra (gpt-6-astra)", "6 Sol (gpt-6-sol)"]);
    expect(container.querySelector(".slash-picker-current-marker")?.closest("li")?.textContent).toContain("6 Astra");
  });

  it("choosing a model sends the agent's own value and the band says what the agent now runs", () => {
    const { container } = live();
    press(container, "M");
    answerList();
    choose(container, "6 Sol");
    expect(last("set_model").model).toBe("gpt-6-sol");
    dispatch({ kind: "settings_result", requestId: last("set_model").request_id, tab: 1, applied: { model: "gpt-6-sol", effort: "low" } });
    expect(flash(container)).toBe("model: gpt-6-sol");
  });

  it("<leader>E lists the agent's levels, with no row for going back to the settings", () => {
    const { container } = live();
    press(container, "E");
    answerList();
    expect(rows(container)).toEqual(["Low (low)", "High (high)"]);
    choose(container, "Low");
    expect(last("apply_effort").effort).toBe("low");
  });

  it("a level the agent itself calls auto is sent as it is, not as the settings row", () => {
    const { container } = live();
    press(container, "E");
    answerList(MODELS, [{ value: "auto", displayName: "Auto", description: "" }, ...EFFORTS]);
    choose(container, "Auto");
    expect(last("apply_effort").effort).toBe("auto");
  });

  it("an agent that lists no models, or no levels, is said to", () => {
    const { container } = live();
    press(container, "M");
    answerList([], []);
    expect(picker(container)).toBeNull();
    expect(flash(container)).toBe("gem reports no models to choose from");
    wait(TYPING_GUARD_MS + 50);
    press(container, "E");
    answerList([], [], { model: "", effort: "" });
    expect(flash(container)).toBe("gem reports no effort levels to choose from");
  });

  it("a click on the band opens the picker", () => {
    const { container } = live({ effort: "high" });
    fireEvent.click(container.querySelector("button.band-effort")!);
    expect(ofType("list_models")).toHaveLength(1);
    answerList();
    expect(rows(container)).toEqual(["Low (low)", "High (high)"]);
  });

  it("a typed /model <name> changes it between turns and is never sent to the agent as a prompt", () => {
    const { container } = live({ activeTurnId: null });
    typeIn(container, "/model gpt-6-sol");
    expect(last("set_model").model).toBe("gpt-6-sol");
    expect(ofType("send_message")).toEqual([]);
    expect(ofType("queue_message")).toEqual([]);
    expect(container.querySelector("textarea")!.value).toBe("");
  });

  it("a typed bare /model opens the picker, between turns too", () => {
    const { container } = live({ activeTurnId: null });
    typeIn(container, "/model");
    expect(ofType("list_models")).toHaveLength(1);
    expect(ofType("send_message")).toEqual([]);
  });

  it("a typed /effort <level> is checked against the agent's levels; one it did not list is named, not sent", () => {
    const { container } = live({ activeTurnId: null });
    typeIn(container, "/effort low");
    answerList();
    expect(last("apply_effort").effort).toBe("low");
    posted.length = 0;
    typeIn(container, "/effort lowest");
    answerList();
    expect(ofType("apply_effort")).toEqual([]);
    expect(ofType("send_message")).toEqual([]);
    expect(ofType("queue_message")).toEqual([]);
    expect(flash(container)).toBe("gem offers low, high");
  });

  it("a refusal from the agent reaches the band in the agent's words", () => {
    const { container } = live();
    press(container, "M");
    answerList();
    choose(container, "6 Sol");
    dispatch({ kind: "command_result", requestId: last("set_model").request_id, ok: false, error: "gem would not make that change: nope" });
    expect(flash(container)).toBe("gem would not make that change: nope");
  });

  it("a Codex tab still offers no choice", () => {
    const { container } = live({}, "codex");
    press(container, "M");
    expect(ofType("list_models")).toEqual([]);
    expect(flash(container)).toBe("Choosing the model of a codex session is not available");
  });
});
