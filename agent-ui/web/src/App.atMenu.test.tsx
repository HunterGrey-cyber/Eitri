// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const sent: Record<string, unknown>[] = [];
beforeEach(() => {
  sent.length = 0;
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (json: string) => sent.push(JSON.parse(json)) } },
  };
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
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
  account: null,
};

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const box = (c: HTMLElement) => c.querySelector<HTMLTextAreaElement>(".composer textarea");
const menuNames = (c: HTMLElement) =>
  Array.from(c.querySelectorAll(".slash-menu-row .slash-menu-name")).map((n) => n.textContent);

/** Tab 1 of `agent` live and idle, `i` into INPUT. */
function inInput(agent: string | undefined, state: Partial<AgentUiState> = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [agent === undefined ? TAB : { ...TAB, agent }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "idle" }, ...state } });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  wait(TYPING_GUARD_MS + 50);
  fireEvent.keyDown(root(rendered.container), { key: "i" });
  wait(TYPING_GUARD_MS + 50);
  expect(box(rendered.container)).not.toBeNull();
  return rendered;
}

function type(c: HTMLElement, value: string) {
  const textarea = box(c)!;
  fireEvent.change(textarea, { target: { value } });
  textarea.setSelectionRange(value.length, value.length);
  fireEvent.select(textarea);
}

describe("the Claude path menu bridge", () => {
  it("sends the query for its tab and drops a late older reply without replacing current rows", () => {
    const { container } = inInput("claude");
    type(container, "@sm");
    wait(60);
    const first = sent.filter((message) => message.type === "path_complete").at(-1)!;
    expect(first).toMatchObject({ type: "path_complete", tab: 1, query: "sm" });
    dispatch({ kind: "path_completions", tab: 1, seq: first.seq, items: ["small.txt"] });
    expect(menuNames(container)).toEqual(["small.txt"]);
    type(container, "@lo");
    wait(60);
    const second = sent.filter((message) => message.type === "path_complete").at(-1)!;
    dispatch({ kind: "path_completions", tab: 1, seq: second.seq, items: ["long.txt", "blob.bin"] });
    dispatch({ kind: "path_completions", tab: 1, seq: first.seq, items: ["small.txt"] });
    expect(menuNames(container)).toEqual(["long.txt", "blob.bin"]);
    fireEvent.keyDown(box(container)!, { key: "Escape" });
    expect(box(container)).not.toBeNull();
    expect(container.querySelector('[aria-label="Files"]')).toBeNull();
  });
  it.each(["codex", "acp:demo"])("never asks for project paths in a %s tab", (agent) => {
    const { container } = inInput(agent);
    type(container, "@sm");
    wait(120);
    expect(sent.filter((message) => message.type === "path_complete")).toEqual([]);
    expect(container.querySelector('[aria-label="Files"]')).toBeNull();
  });
});
