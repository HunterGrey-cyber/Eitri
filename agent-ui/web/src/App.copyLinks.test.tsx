// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

let posted: Array<Record<string, unknown>>;
let writeText: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  writeText = vi.fn();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (window as unknown as { webkit?: unknown }).webkit;
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});

function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}

function started(text?: string) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], expectedVerdandiRevision: null, account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null }] });
  const state = initialState();
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...state, status: { kind: "running" }, ...(text === undefined ? {} : { transcript: [{ seq: 1, text }] }) } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  const box = (el: Element) => {
    el.getBoundingClientRect = () => ({ top: 0, bottom: 100, left: 0, right: 100, width: 100, height: 100, x: 0, y: 0 }) as DOMRect;
  };
  box(rendered.container.querySelector(".agent-ui-conversation")!);
  box(rendered.container.querySelector(".message-list")!);
  rendered.container.querySelectorAll(".row a[href]").forEach(box);
  return rendered;
}

it("copies a detail row to the clipboard and primary selection", () => {
  const { container } = started();
  fireEvent.click(container.querySelector(".band-open")!);
  dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "cwd", value: "/p" }] });
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "y" });
  expect(writeText).toHaveBeenCalledWith("/p");
  expect(posted).toContainEqual({ type: "copy_primary", text: "/p" });
});

it("copies nothing on the detail popover's trailing action", () => {
  const { container } = started();
  fireEvent.click(container.querySelector(".band-open")!);
  dispatch({ kind: "tab_detail", tab: 1, rows: [] });
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "y" });
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.filter((msg) => msg.type === "copy_primary")).toEqual([]);
});

it("holds a disguised mailto click and lets its second click through", () => {
  const { container } = started("[support@example.org](mailto:attacker@example.net?subject=hi)");
  const anchor = container.querySelector(".row a[href]")!;
  vi.setSystemTime(10000);
  expect(fireEvent.click(anchor)).toBe(false);
  vi.setSystemTime(10100);
  expect(fireEvent.click(anchor)).toBe(false);
  vi.setSystemTime(11000);
  expect(fireEvent.click(anchor)).toBe(true);
});

it("lets a mailto click through when its text shows the address", () => {
  const { container } = started("[a@example.com](mailto:a@example.com)");
  expect(fireEvent.click(container.querySelector(".row a[href]")!)).toBe(true);
});
