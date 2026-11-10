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

describe("gf label key admission", () => {
  function pick(many = false) {
    const paths = many ? Array.from({ length: 40 }, (_, i) => `src/file${i}.rs`).join(" ") : "src/first.rs src/second.rs";
    const view = setup(state({ userPrompts: [], transcript: [{ seq: 1, text: paths }] }));
    const open = () => { view.key("g"); view.key("f"); };
    open();
    expect(view.container.querySelector(".path-pick")).not.toBeNull();
    return { ...view, open };
  }
  it.each([{ ctrlKey: true }, { altKey: true }, { metaKey: true }, { super: true }])("cancels without opening for %j", (props) => {
    const { key, container, root } = pick();
    if ("super" in props) fireEvent.keyDown(root, { key: "Super", code: "OSLeft" });
    key("a", props);
    expect(posted.filter((message) => message.type === "open_path")).toEqual([]);
    expect(container.querySelector(".path-pick")).toBeNull();
    fireEvent.keyUp(root, { key: "Super", code: "OSLeft" });
  });
  it("a repeat of the gf key itself keeps the pick open", () => {
    const { key, container } = pick();
    key("f", { repeat: true });
    expect(container.querySelector(".path-pick")).not.toBeNull();
    expect(posted.filter((message) => message.type === "open_path")).toEqual([]);
    key("a");
    expect(posted.filter((message) => message.type === "open_path")).toMatchObject([{ path: "src/first.rs" }]);
  });
  it("a plain label opens exactly its path", () => {
    const { key, container } = pick();
    key("a");
    expect(posted.filter((message) => message.type === "open_path")).toMatchObject([{ path: "src/first.rs" }]);
    expect(container.querySelector(".path-pick")).toBeNull();
  });
  it("Backspace edits a multi-letter label before the plain suffix opens it", () => {
    const { key, container } = pick(true);
    key("a");
    expect(container.querySelector(".path-pick .hint-typed")?.textContent).toBe("a");
    key("Backspace");
    expect(container.querySelector(".path-pick .hint-typed")?.textContent).toBe("");
    key("a"); key("s");
    expect(posted.filter((message) => message.type === "open_path")).toMatchObject([{ path: "src/file1.rs" }]);
  });
});
