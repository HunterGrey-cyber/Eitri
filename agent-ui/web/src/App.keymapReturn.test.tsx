// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

type Posted = { type?: string; opening?: number; return_keys?: boolean };
let posted: Posted[] = [];

afterEach(cleanup);
beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  Element.prototype.scrollIntoView = vi.fn();
});

function dispatch(payload: unknown) {
  act(() => { window.__eitriDispatch!(JSON.stringify(payload)); });
}

function closes(): Posted[] {
  return posted.filter((m) => m.type === "keymap_closed");
}

function shown() {
  const { container } = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"],
    resumableSessions: [], account: "team" });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live",
    mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude" }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } });
  return container;
}

/** The panel's own `F1` (no envelope from Rust). */
function openOwn(container: HTMLElement) {
  fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "F1" });
  return sheetOf(container);
}

function sheetOf(container: HTMLElement) {
  const sheet = container.querySelector<HTMLDivElement>(".keymap-overlay");
  expect(sheet).not.toBeNull();
  return sheet!;
}

describe("closing the sheet prefix ? opened", () => {
  it.each(["Escape", "q", "?"])("%s posts one keymap_closed that returns the keys", (key) => {
    const container = shown();
    dispatch({ kind: "open_keymap", opening: 7 });
    fireEvent.keyDown(sheetOf(container), { key });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ type: "keymap_closed", opening: 7, return_keys: true })]);
  });

  it("stays up through the focus report that follows it, so Esc still returns the keys", () => {
    const container = shown();
    // prefix ? from another module: the chat did not hold the keys, and the report that it does comes after.
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "open_keymap", opening: 7 });
    dispatch({ kind: "pane_focus", focused: true });
    fireEvent.keyDown(sheetOf(container), { key: "Escape" });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ type: "keymap_closed", opening: 7, return_keys: true })]);
  });

  it("stays up over an empty tab through the focus report that follows it, so Esc still returns the keys", () => {
    const container = shown();
    dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "not_started",
      mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude" }] });
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "open_keymap", opening: 8 });
    dispatch({ kind: "pane_focus", focused: true });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    fireEvent.keyDown(document.activeElement ?? sheetOf(container), { key: "Escape" });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ type: "keymap_closed", opening: 8, return_keys: true })]);
  });

  it("closes when the keys leave, returning nothing", () => {
    const container = shown();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "open_keymap", opening: 7 });
    dispatch({ kind: "pane_focus", focused: false });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ opening: 7, return_keys: false })]);
  });

  it("a click outside the sheet posts one keymap_closed that returns the keys", () => {
    const container = shown();
    dispatch({ kind: "open_keymap", opening: 7 });
    fireEvent.click(sheetOf(container));
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ opening: 7, return_keys: true })]);
  });

  it("a close another route forced returns nothing", () => {
    const container = shown();
    dispatch({ kind: "open_keymap", opening: 7 });
    dispatch({ kind: "open_command_line" });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([expect.objectContaining({ opening: 7, return_keys: false })]);
  });

  it("posts the close once, however many renders follow", () => {
    const container = shown();
    dispatch({ kind: "open_keymap", opening: 7 });
    fireEvent.keyDown(sheetOf(container), { key: "Escape" });
    dispatch({ kind: "open_command_line" });
    expect(closes()).toHaveLength(1);
  });

  it("an envelope with no opening reports nothing", () => {
    const container = shown();
    dispatch({ kind: "open_keymap" });
    fireEvent.keyDown(sheetOf(container), { key: "Escape" });
    expect(closes()).toEqual([]);
  });

  it("the panel's own F1 posts nothing on close", () => {
    const container = shown();
    fireEvent.keyDown(openOwn(container), { key: "Escape" });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([]);
  });

  it("a key close of the panel's own F1, then prefix ? forced shut, posts only the latter, as not returned", () => {
    const container = shown();
    fireEvent.keyDown(openOwn(container), { key: "Escape" });
    expect(closes()).toEqual([]);
    dispatch({ kind: "open_keymap", opening: 8 });
    dispatch({ kind: "open_command_line" });
    expect(closes()).toEqual([expect.objectContaining({ opening: 8, return_keys: false })]);
  });
});

describe("prefix ? over the sheet the chat's F1 opened", () => {
  it("moves the open sheet to the prefix table", () => {
    const container = shown();
    const scroll = vi.fn();
    Element.prototype.scrollIntoView = scroll;
    const sheet = openOwn(container);
    expect(scroll).toHaveBeenCalledTimes(1);
    expect((scroll.mock.instances[0] as unknown as HTMLElement).textContent).toBe("This panel");
    scroll.mockClear();
    dispatch({ kind: "open_keymap", opening: 7 });
    expect(container.querySelector(".keymap-overlay"), "the same sheet stays up").toBe(sheet);
    expect(scroll).toHaveBeenCalledTimes(1);
    expect((scroll.mock.instances[0] as unknown as HTMLElement).textContent).toMatch(/^After /);
  });
});
