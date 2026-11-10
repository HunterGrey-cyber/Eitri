// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import { CARD_ARM_MS } from "./cardMode";

let messages: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers({
    toFake: ["Date", "performance", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"],
  });
  messages = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
function pause() { act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50)); }
/** Past card mode's paint and its arming. */
function arm() { act(() => vi.advanceTimersByTime(CARD_ARM_MS + 100)); }
const answers = () => messages.filter((m) => m.type === "permission_response");
function setup() {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 1, resumable: false, failure: null, title: null }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: { ...initialState(), status: { kind: "running" }, pendingPermissions: [{ seq: 1, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }] } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "focus_permission", tab: 1 });
  pause();
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  expect(view.container.querySelector(".row-current .permission-card")).not.toBeNull();
  return { ...view, root };
}

describe("card keys after focus recovery", () => {
  it("a repeated a on body refocuses without answering the waiting card", () => {
    const { root } = setup();
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "a", code: "KeyA", repeat: true });
    fireEvent.keyUp(root, { key: "a", code: "KeyA" });
    pause();
    expect(document.activeElement).toBe(root);
    expect(answers()).toEqual([]);
    fireEvent.keyDown(root, { key: "a", code: "KeyA" });
    fireEvent.keyUp(root, { key: "a", code: "KeyA" });
    pause();
    expect(answers()).toEqual([]);
  });

  /* In card mode a key that landed on <body> is replayed on the root, and card mode never answers a replayed key,
     repeat or not: only a fresh press on the root, once armed again, does. */
  it.each([[true], [false]])("in card mode, a 1 on body (repeat %s) refocuses without answering; a fresh 1 on the root then answers", (repeat) => {
    const { container, root } = setup();
    fireEvent.keyDown(root, { key: "Enter", code: "Enter" });
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    arm();
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "1", code: "Digit1", repeat });
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    pause();
    expect(document.activeElement).toBe(root);
    expect(answers()).toEqual([]);
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    arm();
    fireEvent.keyDown(root, { key: "1", code: "Digit1" });
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    pause();
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "allow" })]);
  });

  it.each([{ isComposing: true }, { keyCode: 229 }])("recovery preserves IME metadata %j", (init) => {
    const { root } = setup();
    act(() => (document.activeElement as HTMLElement).blur());
    const replayed: KeyboardEvent[] = [];
    root.addEventListener("keydown", (event) => replayed.push(event));
    fireEvent.keyDown(document.body, { key: "a", ...init });
    pause();
    expect(replayed).toHaveLength(1);
    expect(replayed[0]).toMatchObject({ repeat: false, ...init });
    expect(answers()).toEqual([]);
  });

  it("IME Escape with keyCode 229 keeps focus in the reason field", () => {
    const { container, root } = setup();
    const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
    act(() => reason.focus());
    expect(fireEvent.keyDown(reason, { key: "Escape", keyCode: 229 })).toBe(true);
    expect(document.activeElement).toBe(reason);
    fireEvent.keyDown(reason, { key: "Escape" });
    expect(document.activeElement).toBe(root);
    expect(answers()).toEqual([]);
  });
});
