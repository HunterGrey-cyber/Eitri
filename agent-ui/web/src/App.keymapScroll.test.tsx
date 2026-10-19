// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

afterEach(cleanup);
beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => undefined } },
  };
  Element.prototype.scrollIntoView = vi.fn();
});

function dispatch(payload: unknown) {
  act(() => { window.__eitriDispatch!(JSON.stringify(payload)); });
}

function openSheet(live: boolean) {
  const { container } = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"],
    resumableSessions: [], expectedVerdandiRevision: null, account: "team" });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null,
    state: live ? "live" : "not_started", mode: "auto", marker: null, pending: 0, resumable: true,
    failure: null, title: null, agent: "claude" }] });
  if (live) dispatch({ kind: "snapshot", tab: 1, throughRevision: 0,
    state: { ...initialState(), status: { kind: "running" } } });
  dispatch({ kind: "open_keymap" });
  const sheet = container.querySelector<HTMLDivElement>(".keymap-overlay")!;
  expect(sheet).not.toBeNull();
  sheet.style.lineHeight = "20px";
  Object.defineProperties(sheet, { clientHeight: { value: 600 }, scrollHeight: { value: 3000 } });
  sheet.scrollTop = 1000;
  return { container, sheet };
}

describe.each([true, false])("keymap sheet scrolling (live=%s)", (live) => {
  it.each<[string, boolean, number]>([
    ["j", false, 1060], ["k", false, 940], ["ArrowDown", false, 1060], ["ArrowUp", false, 940],
    ["d", true, 1300], ["u", true, 700], ["f", true, 1600], ["b", true, 400],
    ["PageDown", false, 1600], ["PageUp", false, 400], [" ", false, 1600], ["G", false, 2400],
  ])("%s (ctrl=%s) scrolls the sheet", (key, ctrlKey, want) => {
    const { sheet } = openSheet(live);
    expect(fireEvent.keyDown(sheet, { key, ctrlKey, shiftKey: key === "G" })).toBe(false);
    expect(sheet.scrollTop).toBe(want);
  });

  it("gg goes to the top, preserving the sequence across a bare modifier", () => {
    const { sheet } = openSheet(live);
    fireEvent.keyDown(sheet, { key: "g" });
    expect(sheet.scrollTop).toBe(1000);
    fireEvent.keyDown(sheet, { key: "Shift" });
    fireEvent.keyDown(sheet, { key: "g" });
    expect(sheet.scrollTop).toBe(0);
  });

  it.each(["?", "Escape", "q"])("%s closes the sheet", (key) => {
    const { sheet, container } = openSheet(live);
    fireEvent.keyDown(sheet, { key });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
  });

  it("an intervening key cancels gg, and reopening starts a fresh sequence", () => {
    const { sheet, container } = openSheet(live);
    for (const key of ["g", "x", "g"]) fireEvent.keyDown(sheet, { key });
    expect(sheet.scrollTop).toBe(1000);
    fireEvent.keyDown(sheet, { key: "Escape" });
    dispatch({ kind: "open_keymap" });
    const reopened = container.querySelector<HTMLDivElement>(".keymap-overlay")!;
    reopened.scrollTop = 1000;
    fireEvent.keyDown(reopened, { key: "g" });
    expect(reopened.scrollTop).toBe(1000);
  });
});
