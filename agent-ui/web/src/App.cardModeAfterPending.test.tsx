// @vitest-environment jsdom
/**
 * Card mode entered over a key that waits for its next one: a pending `g`, a waiting `y` operator (bare, and with
 * its `i` scope), and a count. `Alt+n` takes card mode on the waiting card, drops what waited with a flash naming it,
 * and the dropped key does nothing afterwards: no copy, no move, no answer.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import { COUNT_DROPPED_FLASH, pendingDroppedFlash } from "./app/flashes";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;
let writeText: ReturnType<typeof vi.fn>;
let widen: (container: HTMLElement) => void;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  writeText = vi.fn(() => Promise.resolve());
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  vi.useFakeTimers({
    toFake: ["Date", "performance", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"],
  });
  widen = stubBandWidth();
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

/** The band measures its host through a ResizeObserver; here it is wide enough for every segment. */
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

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

/** Tab 1 live, a reply, then a provider's card waiting on its tool call; the keys arrived (BROWSE on the card). */
function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
  const input = { command: "rm build" };
  const events: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "content_delta", turn_id: "t1", kind: "text", text: "looking at `build`\n\n```sh\nls build\n```" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input },
    { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input },
  ];
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: events.length, events });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
const press = (key: string, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
const altN = () => press("n", { altKey: true, code: "KeyN" });
const inCardMode = (c: HTMLElement) => c.querySelector(".permission-card-mode");
const band = (c: HTMLElement) => c.querySelector(".band-message")?.textContent ?? null;
const cardSegment = (c: HTMLElement) => c.querySelector(".band-card")?.textContent ?? null;
const currentRow = (c: HTMLElement) => c.querySelector(".row-current")?.getAttribute("data-row-key") ?? null;
const copies = () => [...writeText.mock.calls.map(([text]) => text), ...posted.filter((m) => m.type === "copy_primary")];
const answers = () => posted.filter((m) => m.type === "permission_response" || m.type === "editor_card_answer");

describe("Alt+n over a key that waits", () => {
  it.each([
    ["g", ["g"], pendingDroppedFlash("g")],
    ["y", ["y"], pendingDroppedFlash("y")],
    ["3", ["3"], COUNT_DROPPED_FLASH],
    ["y i", ["y", "i"], pendingDroppedFlash("y")],
  ])("%s, then Alt+n, enters card mode on the card, says what it dropped, and copies or moves nothing", (_name, keys, flash) => {
    const { container } = started();
    expect(container.querySelector(".row-current.row-permission")).not.toBeNull();
    const onCard = currentRow(container);
    for (const key of keys) press(key);
    expect(altN()).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(cardSegment(container)).toMatch(/^card /);
    expect(band(container)).toBe(flash);
    expect(currentRow(container)).toBe(onCard);
    // Leaving card mode, the dropped key is gone: `k` is a plain move, not the end of `yk` or `3k`.
    press("Escape");
    wait(TYPING_GUARD_MS + 50);
    expect(inCardMode(container)).toBeNull();
    press("k");
    wait(TYPING_GUARD_MS + 50);
    expect(copies()).toEqual([]);
    expect(answers()).toEqual([]);
  });
});
