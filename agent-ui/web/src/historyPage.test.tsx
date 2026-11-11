// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { parseExCommand } from "./trust";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import { conversationRows, stopsIn } from "./nav";
import * as history from "./historyPage";
import css from "./index.css?raw";

let posted: Array<Record<string, unknown>>;
let widen: (container: HTMLElement) => void;
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });
beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  widen = stubBandWidth();
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); });
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const press = (c: HTMLElement, key: string) => fireEvent.keyDown(root(c), { key });
const pause = () => act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
const rows = (c: HTMLElement) => conversationRows(c);
const keys = (c: HTMLElement) => rows(c).map((r) => r.getAttribute("data-row-key"));
const old = { ...initialState(), userPrompts: [{ seq: 0, text: "archived prompt" }], transcript: [{ seq: 1, text: "archived reply <script>bad()</script>" }] };
const page = (fromTurn = 2) => dispatch({ kind: "history_page", tab: 1, requestId: "load", fromTurn, toTurn: fromTurn + 1, records: [old] });
const clear = () => dispatch({ kind: "history_page", tab: 1, cleared: true });
function live() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1", state: "live", mode: "auto", pending: 0, resumable: true }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 20, state: { ...initialState(), status: { kind: "running" }, backend: "sidecar",
    evicted: { throughSeq: 40, turns: 3 }, userPrompts: [{ seq: 40, text: "live prompt" }], transcript: [{ seq: 41, text: "live reply" }] } });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true }); dispatch({ kind: "arrive" });
  act(() => widen(rendered.container)); pause();
  return rendered;
}
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


it("prepends h: rows above the live tail, counts fromTurn, sanitizes text and clears", () => {
  const { container } = live();
  const before = keys(container);
  page();
  expect(keys(container)).toEqual(["h:u-0", "h:m-1", ...before]);
  expect(container.textContent).toContain("2 earlier turns not loaded.");
  expect(container.querySelector(".message-list script")).toBeNull();
  page(0);
  expect(container.textContent).not.toContain("earlier turns not loaded.");
  clear();
  expect(keys(container)).toEqual(before);
  expect(container.textContent).toContain("3 earlier turns not loaded.");
});
it("drops another tab's page and clear while preserving the active tab's page", () => {
  const { container } = live();
  const before = keys(container);
  dispatch({ kind: "history_page", tab: 2, requestId: "foreign", fromTurn: 0, records: [old] });
  expect(keys(container)).toEqual(before);
  page();
  const held = keys(container);
  dispatch({ kind: "history_page", tab: 2, cleared: true });
  expect(keys(container)).toEqual(held);
});
it("Enter on the notice and both ex commands post the named tab and action", () => {
  const { container } = live();
  const button = container.querySelector<HTMLButtonElement>(".history-notice button")!;
  expect(stopsIn(container)).toContain(button.parentElement);
  act(() => button.focus());
  // jsdom does not synthesize the browser's native button activation after an unclaimed Enter.
  expect(fireEvent.keyDown(button, { key: "Enter" })).toBe(true);
  fireEvent.click(button);
  for (const action of ["older", "latest"] as const) {
    expect(parseExCommand(`history ${action}`)).toEqual({ command: "history", action });
    dispatch({ kind: "open_command_line" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    fireEvent.change(input, { target: { value: `history ${action}` } });
    fireEvent.keyDown(input, { key: "Enter" });
  }
  expect(posted.filter((m) => m.type === "history_request")).toEqual([
    expect.objectContaining({ tab: 1, action: "older", request_id: expect.any(String) }),
    expect.objectContaining({ tab: 1, action: "older" }), expect.objectContaining({ tab: 1, action: "latest" }),
  ]);
  expect(parseExCommand("history older extra")).toBeNull();
  expect(parseExCommand("history newest")).toBeNull();
});
it("preserves the cursor key across a prepend and clamps a released historical cursor to the first live row", () => {
  const { container } = live();
  press(container, "g"); press(container, "g");
  const key = container.querySelector(".row-current")!.getAttribute("data-row-key");
  page();
  expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(key);
  press(container, "g"); press(container, "g");
  press(container, "j");
  expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe("h:m-1");
  clear();
  expect(container.querySelector(".row-current")).toBe(rows(container)[0]);
});
it("c on historical text still reviews the latest live turn", () => {
  const { container } = live();
  page(); press(container, "g"); press(container, "g"); pause();
  press(container, "c");
  expect(posted.filter((m) => m.type === "review_request")).toEqual([
    expect.objectContaining({ tab: 1, turn: "latest", scope: "turn" }),
  ]);
});
it("historical records never raise cards or answer by a or Enter", () => {
  const { container } = live();
  const archived = { ...old,
    toolCalls: [
      { ...tool(2), name: "Write", result: null },
      { ...tool(4), name: "Write", result: null },
      { ...tool(6), name: "AskUserQuestion", result: null },
    ],
    pendingPermissions: [{ seq: 3, permissionId: "old-card", toolUseId: "call-2", toolName: "Write", input: {} }],
    editorCards: [{ seq: 5, permissionId: "old-editor", toolUseId: "call-4", toolName: "apply_proposal", input: null,
      editorCard: { title: "Archived proposal", body: [] } }],
    questions: [{ seq: 7, interactionId: "old-question", toolUseId: "call-6", turnId: "old", unsupportedReason: null,
      state: { kind: "waiting" }, questions: [{ question: "Archived question?", header: "Question", multiSelect: false,
        options: [{ label: "Yes", description: "", preview: null }] }] }],
  };
  dispatch({ kind: "history_page", tab: 1, requestId: "cards", fromTurn: 2, records: [archived] });
  expect(keys(container).filter((k) => k?.startsWith("h:"))).toEqual(["h:u-0", "h:m-1", "h:t-2", "h:t-4", "h:t-6"]);
  expect(container.querySelector(".permission-card, .question-card")).toBeNull();
  expect(container.textContent).not.toContain("Archived proposal");
  expect(container.textContent).not.toContain("Archived question?");
  press(container, "g"); press(container, "g");
  posted.length = 0;
  for (let i = 0; i < 5; i++) {
    for (const key of ["a", "Enter"]) { pause(); press(container, key); pause(); }
    press(container, "j");
  }
  expect(posted).toEqual([]);
});
it.each(["older", "latest"])("a refused history %s flashes the host's words in the footer", (action) => {
  const { container } = live();
  dispatch({ kind: "open_command_line" });
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  fireEvent.change(input, { target: { value: `history ${action}` } });
  fireEvent.keyDown(input, { key: "Enter" });
  const request = posted.find((m) => m.type === "history_request")!;
  dispatch({ kind: "command_result", requestId: request.request_id, ok: false, error: "moving: keep the host reason" });
  expect(container.querySelector(".status-band")?.textContent).toContain("moving: keep the host reason");
  expect(container.querySelector(".command-notice")).toBeNull();
});
it("the reload notice is a plain-text button and remains a navigation stop", () => {
  const { container } = live();
  const button = container.querySelector<HTMLButtonElement>(".history-notice button")!;
  expect(button.className).toBe("history-load");
  expect(stopsIn(container)).toContain(button.parentElement);
  act(() => button.focus());
  expect(document.activeElement).toBe(button);
  const style = document.createElement("style");
  style.textContent = css;
  document.head.append(style);
  try {
    button.blur();
    const drawn = getComputedStyle(button);
    expect(drawn.marginLeft).toBe("0px");
    expect(drawn.textDecoration).toBe("none");
    expect(drawn.backgroundColor).toBe("rgba(0, 0, 0, 0)");
    expect(drawn.borderWidth).toBe("0px");
  } finally { style.remove(); }
});
const tool = (seq: number) => ({ seq, turnId: "old", toolUseId: `call-${seq}`, name: "Read", input: { file_path: `file-${seq}` }, result: { content: "ok", isError: false } });
function toolPage() {
  dispatch({ kind: "history_page", tab: 1, requestId: "tools", fromTurn: 2,
    records: [{ ...old, toolCalls: [tool(2), tool(3)] }] });
}
it("live expansion leaves the historical page memo untouched", () => {
  const spy = vi.spyOn(history, "historyTimeline");
  try {
    const { container } = live();
    toolPage();
    dispatch({ kind: "events", tab: 1, fromRevision: 21, throughRevision: 22, events: [
      { type: "tool_call_started", turn_id: "live", tool_use_id: "live-call", name: "Read", input: {} },
    ] });
    press(container, "G"); pause();
    const before = spy.mock.calls.length;
    press(container, "Enter");
    expect(spy).toHaveBeenCalledTimes(before);
    pause(); press(container, "Enter");
    expect(spy).toHaveBeenCalledTimes(before);
    press(container, "g"); press(container, "g"); press(container, "j"); press(container, "j"); pause();
    press(container, "Enter");
    expect(spy.mock.calls.length).toBeGreaterThan(before);
  } finally { spy.mockRestore(); }
});
it("expanding an archived run keeps the cursor on its first tool", () => {
  const { container } = live();
  toolPage(); press(container, "g"); press(container, "g"); press(container, "j"); press(container, "j"); pause();
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("h:r-2");
  press(container, "Enter");
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("h:t-2");
});
it("zc on an archived tool closes its archived run and follows the cursor", () => {
  const { container } = live();
  toolPage(); press(container, "g"); press(container, "g"); press(container, "j"); press(container, "j"); pause();
  press(container, "Enter"); pause();
  // Choose the second member so folding must resolve its namespaced key to the run.
  fireEvent.click(container.querySelector('[data-row-key="h:t-3"]')!); pause();
  press(container, "z"); press(container, "c");
  expect(keys(container)).toContain("h:r-2");
  expect(keys(container)).not.toContain("h:t-3");
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("h:r-2");
});
it.each(["V", "v"])("keeps %s selection's frozen historical rows until leaving it", (mode) => {
  const { container } = live();
  const selection = window.getSelection()!;
  const modify = vi.fn((alter: string, direction: string) => {
    const node = selection.focusNode;
    if (node === null) return;
    const offset = direction === "backward" ? 0 : (node.textContent?.length ?? 0);
    if (alter === "move") selection.collapse(node, offset);
    else selection.setBaseAndExtent(selection.anchorNode!, selection.anchorOffset, node, offset);
  });
  Object.defineProperty(selection, "modify", { value: modify, configurable: true });
  page(); press(container, "g"); press(container, "g"); pause();
  fireEvent.keyDown(root(container), { key: mode, shiftKey: mode === "V" });
  if (mode === "v") press(container, "v");
  expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe(mode === "V" ? "vline" : "visual");
  const before = keys(container);
  expect(before.some((k) => k?.startsWith("h:"))).toBe(true);
  clear();
  expect(keys(container)).toEqual(before);
  press(container, "Escape"); press(container, "Escape");
  expect(keys(container).every((k) => !k?.startsWith("h:"))).toBe(true);
  window.getSelection()?.removeAllRanges();
});

it("search and copy include the loaded rows and G still lands on the live tail", () => {
  const { container } = live();
  const copy = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText: copy }, configurable: true });
  page(); press(container, "G");
  expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe("m-41");
  pause(); press(container, "/");
  const search = container.querySelector<HTMLInputElement>(".search-bar input")!;
  fireEvent.change(search, { target: { value: "archived prompt" } });
  fireEvent.keyDown(search, { key: "Enter" });
  expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe("h:u-0");
  pause(); press(container, "y"); press(container, "y");
  expect(copy).toHaveBeenCalledWith("archived prompt");
});

const band = (c: HTMLElement) => c.querySelector(".band-message")?.textContent ?? "";
it("keeps paging with the top control and draws a plain gap between historical and live rows", () => {
  const { container } = live(); page(1);
  const gap = container.querySelector(".history-gap")!;
  expect(gap?.textContent).toBe("1 newer turn not loaded.");
  expect(gap?.previousElementSibling?.getAttribute("data-row-key")).toBe("h:m-1");
  expect(gap?.nextElementSibling?.getAttribute("data-row-key")).toBe("u-40");
  expect(gap?.querySelector("button")).toBeNull();
  expect(stopsIn(container)).not.toContain(gap);
  expect(rows(container)).not.toContain(gap);
  const button = container.querySelector<HTMLButtonElement>(".history-load")!;
  act(() => button.focus());
  expect(fireEvent.keyDown(button, { key: "Enter" })).toBe(true);
  fireEvent.click(button);
  expect(posted.at(-1)).toMatchObject({ type: "history_request", tab: 1, action: "older" });
  page(0);
  expect(container.querySelector(".history-load")).toBeNull();
  expect(container.querySelector(".history-gap")?.textContent).toBe("2 newer turns not loaded.");
  page(2); expect(container.querySelector(".history-gap")).toBeNull();
  page(0); clear(); expect(container.querySelector(".history-gap")).toBeNull();
});
it.each(["latest", "reset", "switch"])("clears the gap silently on %s", (how) => {
  const { container } = live(); page(0);
  if (how === "latest") clear();
  if (how === "reset") dispatch({ kind: "events", tab: 1, events: [{ type: "conversation_reset", new_conversation_id: "new", trigger: null }] });
  if (how === "switch") dispatch({ kind: "tabs", active: 2, tabs: [{ id: 2, number: 2, label: "2", state: "live", mode: "auto", pending: 0, resumable: true }] });
  expect(container.querySelector(".history-gap")).toBeNull();
  expect(band(container)).not.toContain("unloaded"); expect(band(container)).not.toContain("released");
});
it.each([false, true])("flashes an empty read once with held=%s and ignores another tab's result", (heldPage) => {
  const { container } = live(); if (heldPage) page(1); const held = keys(container);
  dispatch({ kind: "history_page", tab: 2, requestId: "foreign", empty: true });
  expect(band(container)).not.toContain("No older");
  dispatch({ kind: "history_page", tab: 1, requestId: "empty", empty: true });
  expect(band(container)).toBe("No older turns could be loaded."); expect(keys(container)).toEqual(held);
  act(() => vi.advanceTimersByTime(10000)); expect(band(container)).not.toContain("No older");
});
it.each([1, 8])("flashes only an automatic page release using envelope count %s despite a stale page base", (turns) => {
  const { container } = live(); page(1);
  dispatch({ kind: "history_page", tab: 1, cleared: true, reason: "released", turns });
  expect(band(container)).toBe(`The loaded page was released to make room; ${turns} earlier turn${turns === 1 ? "" : "s"} not loaded.`);
  expect(keys(container).every((k) => !k?.startsWith("h:"))).toBe(true);
  act(() => vi.advanceTimersByTime(10000)); page(1); page(0); clear();
  expect(band(container)).not.toContain("released"); expect(band(container)).not.toContain("cursor moved");
});
it("flashes once when eviction removes the cursor's row, but not a surviving row", () => {
  const { container } = live(); press(container, "g"); press(container, "g");
  dispatch({ kind: "events", tab: 1, events: [{ type: "history_evicted", through_seq: 41, turns: 4, lifted: [] }] });
  expect(band(container)).toBe("Earlier turns were unloaded; the cursor moved to the first loaded row.");
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("m-41");
  act(() => vi.advanceTimersByTime(10000));
  dispatch({ kind: "events", tab: 1, events: [{ type: "history_evicted", through_seq: 41, turns: 5, lifted: [] }] });
  expect(band(container)).not.toContain("cursor moved");
});
it.each(["/", "?"])("%s and n/N explain search misses using all unloaded turns, including the gap", (key) => {
  const { container } = live(); page(0); pause(); press(container, key);
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  fireEvent.change(input, { target: { value: "absent" } }); fireEvent.keyDown(input, { key: "Enter" });
  const expected = "pattern not found in loaded turns: absent (2 earlier turns not loaded)";
  expect(band(container)).toBe(expected);
  for (const repeat of ["n", "N"]) { pause(); press(container, repeat); expect(band(container)).toBe(expected); }
  dispatch({ kind: "history_page", tab: 1, requestId: "all", fromTurn: 0, toTurn: 3,
    records: [old, ...[10, 20].map((seq) => ({ ...initialState(), userPrompts: [{ seq, text: `archived ${seq}` }] }))] });
  pause(); press(container, "n"); expect(band(container)).toBe("pattern not found: absent");
  clear(); pause(); press(container, "n");
  expect(band(container)).toBe("pattern not found in loaded turns: absent (3 earlier turns not loaded)");
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 30, state: { ...old, status: { kind: "running" }, evicted: null } });
  pause(); press(container, "n"); expect(band(container)).toBe("pattern not found: absent");
});

it.each(["/", "?"])("region %s misses explain which turns were searched", (direction) => {
  const { container } = live();
  const selection = window.getSelection()!;
  Object.defineProperty(selection, "modify", { value: vi.fn(), configurable: true });
  pause(); press(container, "v");
  expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
  pause(); press(container, direction);
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  fireEvent.change(input, { target: { value: "absent" } });
  fireEvent.keyDown(input, { key: "Enter" });
  expect(band(container)).toBe("pattern not found in loaded turns: absent (3 earlier turns not loaded)");
  selection.removeAllRanges();
});

it("silently clamps a cursor on a page the user replaced or released", () => {
  const { container } = live(); page(1);
  press(container, "g"); press(container, "g");
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("h:u-0");
  dispatch({ kind: "history_page", tab: 1, requestId: "older", fromTurn: 0, toTurn: 1,
    records: [{ ...initialState(), userPrompts: [{ seq: 10, text: "different archived turn" }] }] });
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("h:u-10");
  expect(band(container)).not.toContain("cursor moved");
  clear();
  expect(container.querySelector(".row-current")?.getAttribute("data-row-key")).toBe("u-40");
  expect(band(container)).not.toContain("cursor moved"); expect(band(container)).not.toContain("released");
});

it("uses singular wording when a search misses with one earlier turn unloaded", () => {
  const { container } = live();
  dispatch({ kind: "history_page", tab: 1, requestId: "two", fromTurn: 1, toTurn: 3,
    records: [old, { ...initialState(), userPrompts: [{ seq: 10, text: "another archived prompt" }] }] });
  pause(); press(container, "/");
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  fireEvent.change(input, { target: { value: "absent" } });
  fireEvent.keyDown(input, { key: "Enter" });
  expect(band(container)).toBe("pattern not found in loaded turns: absent (1 earlier turn not loaded)");
});
