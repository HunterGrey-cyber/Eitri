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
    resumableSessions: [], expectedVerdandiRevision: null, account: "team" });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live",
    mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude" }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } });
  return container;
}

/** The panel's own `?` (no envelope from Rust). */
function openOwn(container: HTMLElement) {
  fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "?", shiftKey: true });
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

  it("the panel's own ? posts nothing on close", () => {
    const container = shown();
    fireEvent.keyDown(openOwn(container), { key: "Escape" });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(closes()).toEqual([]);
  });

  it("a key close of the panel's own ?, then prefix ? forced shut, posts only the latter, as not returned", () => {
    const container = shown();
    fireEvent.keyDown(openOwn(container), { key: "Escape" });
    expect(closes()).toEqual([]);
    dispatch({ kind: "open_keymap", opening: 8 });
    dispatch({ kind: "open_command_line" });
    expect(closes()).toEqual([expect.objectContaining({ opening: 8, return_keys: false })]);
  });
});
