// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import type { PanelActionName, PanelMotionName, PanelTable } from "./keymap";
import { binding } from "./testFixtures";
import { TYPING_GUARD_MS, TypingGuard } from "./typingGuard";

let posted: Record<string, unknown>[];
let resize: { callback: ResizeObserverCallback; targets: Element[] }[];
beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  posted = [];
  resize = [];
  vi.stubGlobal("ResizeObserver", class {
    record: typeof resize[number];
    constructor(callback: ResizeObserverCallback) { this.record = { callback, targets: [] }; resize.push(this.record); }
    observe(el: Element) { this.record.targets.push(el); }
    unobserve() {} disconnect() {}
  });
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});
afterEach(() => {
  cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals();
  delete (window as unknown as { webkit?: unknown }).webkit;
});
const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
const motions: PanelTable = { ...EMPTY_PANEL_TABLE, bindings: [
  binding(["j"], "line.down"), binding(["x"], "row.down"), binding(["]", "t"], "tool.prev"),
] };
function table(panel: PanelTable) {
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel, newTabChord: "Ctrl+b c" });
}
function started(panel = motions) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null }] });
  const state = initialState();
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: {
    ...state, status: { kind: "running" }, capabilities: { ...state.capabilities, directSettings: true },
    userPrompts: [{ seq: 1, text: "first prompt" }, { seq: 2, text: "last prompt" }],
  } });
  dispatch({ kind: "pane_focus", focused: true }); dispatch({ kind: "arrive" });
  table(panel);
  act(() => {
    for (const { callback, targets } of resize) for (const target of targets) {
      if (target.matches(".status-band, .band-measure")) callback([{ target, contentRect: { width: target.matches(".band-measure") ? 7.2 : 1400 } } as ResizeObserverEntry], {} as ResizeObserver);
    }
  });
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const key = (key: string, extra = {}) => fireEvent.keyDown(root, { key, shiftKey: /^[A-Z{}]$/.test(key), ...extra });
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
  return { ...view, root, key };
}

it("3j scrolls three text lines immediately, and repeat uses the same motion", () => {
  const { container, key } = started();
  key("g"); key("g");
  const list = container.querySelector<HTMLElement>(".message-list")!;
  Object.defineProperties(list, { scrollHeight: { configurable: true, value: 3000 }, clientHeight: { configurable: true, value: 300 } });
  for (const body of container.querySelectorAll<HTMLElement>(".row-body")) body.style.lineHeight = "20px";
  list.scrollTop = 0;
  key("3"); key("j");
  expect(list.scrollTop).toBe(60);
  expect(container.querySelector(".which-key")).toBeNull();
  key("j", { repeat: true });
  expect(list.scrollTop).toBe(80);
  expect(posted.some((msg) => msg.type === "tab_verb")).toBe(false);
});

it("the sheet includes the user's motion key and removes it from its old motion", () => {
  const { container, key } = started();
  key("F1");
  const section = Array.from(container.querySelectorAll("section")).find((s) => s.querySelector("h2")?.textContent === "This panel")!;
  const rows = Array.from(section.querySelectorAll("tr"));
  const line = rows.find((row) => row.textContent?.includes("One line down"))!;
  const row = rows.find((row) => row.textContent?.includes("Next row;"))!;
  expect(line.querySelector("kbd")?.textContent).toBe("Ctrl+e / j");
  expect(row.querySelector("kbd")?.textContent).toBe("↓ / x");
});

it("updates one guard's motion keys and cancels a pending HINT wait on a table change", () => {
  const sets = vi.spyOn(TypingGuard.prototype, "setMotionKeys");
  const { key } = started();
  expect(sets.mock.lastCall![0].has("x")).toBe(true);
  key("f");
  act(() => vi.advanceTimersByTime(50));
  table({ ...motions, bindings: [
    binding(["j"], "help"), binding(["G"], "detailed.toggle"),
    binding(["q"], "row.down"), binding(["]", "t"], "tool.prev"),
  ] });
  const keys = sets.mock.lastCall![0];
  for (const key of ["j", "G", "x"]) expect(keys.has(key), key).toBe(false);
  for (const key of ["q", "]", "t", "k", "Tab"]) expect(keys.has(key), key).toBe(true);
  expect(new Set(sets.mock.contexts).size).toBe(1);
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS - 50));
  expect(posted.filter((msg) => msg.type === "hint_request")).toHaveLength(0);
});

it("preserves input-exit arming when the same guard receives new motion keys", () => {
  const { key, container } = started();
  key("i");
  const composer = container.querySelector<HTMLTextAreaElement>("textarea")!;
  fireEvent.keyDown(composer, { key: "Escape" });
  table({ ...motions, bindings: [...motions.bindings, binding(["q"], "row.up")] });
  key("i");
  expect(document.activeElement).toBe(container.querySelector("textarea"));
});

it("a motion on L moves immediately without deferring a tab command", () => {
  const defer = vi.spyOn(TypingGuard.prototype, "defer");
  const { key, container } = started({ ...motions, bindings: [...motions.bindings, binding(["L"], "row.down")] });
  key("g"); key("g");
  defer.mockClear();
  key("L");
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("u-2");
  expect(defer).not.toHaveBeenCalled();
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
  expect(posted.some((msg) => msg.type === "tab_verb")).toBe(false);
});

it("a table change still cancels a deferred command from the old table", () => {
  const { key } = started({ ...motions, bindings: [...motions.bindings, binding(["L"], "tab.next")] });
  key("L");
  table({ ...motions, bindings: [...motions.bindings, binding(["L"], "tab.prev")] });
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
  expect(posted.some((msg) => msg.type === "tab_verb")).toBe(false);
});

const commands: { action: Exclude<PanelActionName, PanelMotionName>; type?: string; verb?: string; selector?: string; flash?: string }[] = [
  ...(["new", "next", "prev", "last", "close", "close-others", "choose", "info"] as const).map((verb) => ({
    action: `tab.${verb}` as const, type: "tab_verb", verb: verb.replace("-", "_"),
  })),
  { action: "panel.search", selector: ".search-bar" },
  { action: "panel.keymap", selector: ".keymap-overlay" },
  { action: "panel.handoff", selector: ".handoff" },
  { action: "mode.cycle", type: "cycle_mode" },
  { action: "model.pick", type: "list_models" },
  { action: "effort.pick", type: "list_models" },
  { action: "tab.stop-background", flash: "this Claude Code sidecar does not report background work" },
  { action: "tab.follow", type: "toggle_following" },
  { action: "remote.devices", type: "remote_devices_open" },
];
it.each(commands)("leader still runs $action alongside motion rows", ({ action, type, verb, selector, flash }) => {
  const { container, key } = started({ ...motions, bindings: [...motions.bindings, binding(["<leader>", "a"], action)] });
  if (action === "remote.devices") dispatch({ kind: "remote_on" });
  posted.length = 0;
  key(" "); key("a");
  if (type) expect(posted).toContainEqual(expect.objectContaining({ type, ...(verb ? { verb } : {}) }));
  if (selector) expect(container.querySelector(selector)).not.toBeNull();
  if (flash) expect(container.textContent).toContain(flash);
});
