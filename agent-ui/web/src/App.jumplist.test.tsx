// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState } from "./types";

const TAB = { id: 1, number: 1, label: "one", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null };
const TWO = [TAB, { ...TAB, id: 2, number: 2, label: "two" }];
const state = (overrides: Partial<AgentUiState> = {}): AgentUiState => ({ ...initialState(), status: { kind: "running" },
  userPrompts: [{ seq: 0, text: "start" }, { seq: 4, text: "another prompt" }],
  transcript: [{ seq: 1, text: "apple above" }, { seq: 2, text: "banana" }, { seq: 3, text: "apple below" }, { seq: 5, text: "last" }], ...overrides });
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
let posted: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers();
  posted = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (message) => posted.push(JSON.parse(message)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); window.getSelection()?.removeAllRanges(); });
function setup(snapshot = state()) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: TWO });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 10, state: snapshot });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const key = (key: string, props = {}, target: Element = root) => {
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    fireEvent.keyDown(target, { key, ...props });
  };
  const current = () => view.container.querySelector<HTMLElement>(".row-current");
  const prompt = () => view.container.querySelector<HTMLInputElement>('[aria-label="Search the conversation"]')!;
  return { ...view, root, key, current, prompt };
}

describe("the tab's jumplist", () => {
  it("G, Ctrl+o and Ctrl+i preserve the row and its viewport offset", () => {
    const { key, current, container } = setup();
    key("g"); key("g"); key("j");
    expect(current()?.textContent).toContain("apple above");
    const list = container.querySelector<HTMLElement>(".message-list")!;
    Object.defineProperties(list, { clientHeight: { configurable: true, value: 150 }, scrollHeight: { configurable: true, value: 900 } });
    list.getBoundingClientRect = () => ({ top: 100, bottom: 250, height: 150 }) as DOMRect;
    const rows = Array.from(list.querySelectorAll<HTMLElement>("[data-row-key]"));
    rows.forEach((row, index) => { row.getBoundingClientRect = () => ({ top: 100 + index * 150 - list.scrollTop, bottom: 250 + index * 150 - list.scrollTop, height: 150 }) as DOMRect; });
    list.scrollTop = 175;
    key("G", { shiftKey: true });
    expect(current()?.textContent).toContain("last");
    key("o", { ctrlKey: true });
    expect(current()?.textContent).toContain("apple above");
    expect(list.scrollTop).toBe(175);
    key("i", { ctrlKey: true });
    expect(current()?.textContent).toContain("last");
    expect(list.scrollTop).toBe(750);
  });

  it("gg, [[ and n each record their departure", () => {
    const { key, current, prompt } = setup();
    key("g"); key("g");
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("last");
    key("i", { ctrlKey: true }); expect(current()?.textContent).toContain("start");
    key("G", { shiftKey: true }); key("["); key("[");
    expect(current()?.textContent).toContain("another prompt");
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("last");
    key("/"); fireEvent.change(prompt(), { target: { value: "apple" } }); key("Enter", {}, prompt());
    expect(current()?.textContent).toContain("apple above");
    key("n"); expect(current()?.textContent).toContain("apple below");
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("apple above");
  });

  it("clamps a vanished row's former index", () => {
    const { key, current } = setup();
    key("g"); key("g"); key("j"); key("G", { shiftKey: true });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 11, state: state({ transcript: [{ seq: 5, text: "last" }] }) });
    key("o", { ctrlKey: true });
    expect(current()?.textContent).toContain("another prompt");
  });

  it("keeps separate lists through tab switches", () => {
    const { key, current } = setup();
    key("g"); key("g"); key("j"); key("G", { shiftKey: true });
    dispatch({ kind: "tabs", active: 2, tabs: TWO });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 10, state: state({ transcript: [{ seq: 1, text: "second middle" }, { seq: 5, text: "second end" }] }) });
    key("g"); key("g");
    dispatch({ kind: "tabs", active: 1, tabs: TWO });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 10, state: state() });
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("apple above");
    dispatch({ kind: "tabs", active: 2, tabs: TWO });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 10, state: state({ transcript: [{ seq: 1, text: "second middle" }, { seq: 5, text: "second end" }] }) });
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("second end");
    expect(posted.filter((message) => JSON.stringify(message).includes("jumplist"))).toEqual([]);
  });

  it("INSERT Ctrl+o and BROWSE zi toggle the same detailed view", () => {
    const { key, container } = setup(state({ thinking: [{ seq: 6, turnId: "t", text: "hidden thought" }] }));
    dispatch({ kind: "display", thinking: "hidden" });
    expect(container.querySelector(".row-thinking")).toBeNull();
    key("z"); key("i"); expect(container.querySelector(".row-thinking")).not.toBeNull();
    key("i");
    key("o", { ctrlKey: true }, container.querySelector("textarea")!);
    expect(container.querySelector(".row-thinking")).toBeNull();
  });
});
