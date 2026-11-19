// @vitest-environment jsdom
/**
 * The composer's `/` menu inside the whole panel: where each kind of tab gets its rows (Claude's built-in
 * table, an ACP agent's own list, nothing for Codex), that a list the agent reports replaces them live, and
 * that Esc closes the menu before it leaves INPUT.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const postMessage = vi.fn<(msg: string) => void>();
beforeEach(() => {
  postMessage.mockClear();
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage } },
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
const mode = (c: HTMLElement) => c.querySelector('[data-testid="mode-block"]')!.textContent;

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

const ACP_COMMANDS = [
  { name: "web", description: "Search the web", hint: "query to search for" },
  { name: "test", description: "Run tests for the current project" },
];

describe("where a tab's / menu rows come from", () => {
  it.each(["tasks", "bashes", "workflows"])("/%s opens Eitri's overlay from a reported command", (name) => {
    const { container } = inInput("claude", { availableCommands: [{ name, description: "CLI command" }] });
    type(container, `/${name}`);
    fireEvent.keyDown(box(container)!, { key: "Enter" });
    expect(container.querySelector('.tasks-overlay[aria-label="Background tasks"]')).not.toBeNull();
  });
  it("a Claude tab offers Claude's built-in commands", () => {
    const { container } = inInput(undefined);
    type(container, "/co");
    expect(menuNames(container)).toEqual(["/compact [instructions]", "/cost", "/context"]);
  });

  it("an ACP tab offers the agent's own list instead, with its argument hint, and none of Claude's", () => {
    const { container } = inInput("acp:demo", { availableCommands: ACP_COMMANDS });
    type(container, "/");
    expect(menuNames(container)).toEqual(["/web query to search for", "/test"]);
    type(container, "/co");
    expect(container.querySelector(".slash-menu")).toBeNull();
  });

  it("an ACP agent's text is drawn as plain text", () => {
    const { container } = inInput("acp:demo", {
      availableCommands: [{ name: "<img/src=x/onerror=alert(1)>", description: "<b>bold</b> [x](http://evil)" }],
    });
    type(container, "/");
    const menu = container.querySelector(".slash-menu")!;
    expect(menu.querySelector("img, b, a")).toBeNull();
    expect(menu.textContent).toContain("<b>bold</b> [x](http://evil)");
  });

  it("an ACP tab that has listed nothing yet, and a Codex tab, have no menu", () => {
    for (const agent of ["acp:demo", "codex"]) {
      const { container, unmount } = inInput(agent);
      type(container, "/");
      expect(container.querySelector(".slash-menu"), agent).toBeNull();
      unmount();
    }
  });

  it("a list the Claude tab reports replaces the built-in table, without changing the menu", () => {
    const { container } = inInput("claude", { availableCommands: [{ name: "my-skill", description: "A user skill" }] });
    type(container, "/");
    expect(menuNames(container)).toEqual(["/my-skill"]);
  });

  it("a list that arrives while the box is open updates the menu, and a later one replaces it", () => {
    const { container } = inInput("acp:demo");
    type(container, "/");
    expect(container.querySelector(".slash-menu")).toBeNull();
    const event = (commands: unknown): { kind: string } & Record<string, unknown> => ({
      kind: "events",
      tab: 1,
      fromRevision: 0,
      throughRevision: 1,
      events: [{ type: "commands_available", commands } as AgentDomainEvent],
    });
    dispatch(event(ACP_COMMANDS));
    expect(menuNames(container)).toEqual(["/web query to search for", "/test"]);
    dispatch({ ...event([{ name: "plan", description: "Plan" }]), fromRevision: 1, throughRevision: 2 });
    expect(menuNames(container)).toEqual(["/plan"]);
  });
});

describe("Esc with the menu open", () => {
  it.each([false, true])("closes the menu then leaves INPUT while syncing is %s", (waiting) => {
    const { container } = inInput(undefined);
    dispatch({ kind: "send_sync", tab: 1, waiting, lines: [] });
    type(container, "/co");
    expect(container.querySelector(".slash-menu")).not.toBeNull();
    fireEvent.keyDown(box(container)!, { key: "Escape" });
    expect(container.querySelector(".slash-menu")).toBeNull();
    expect(box(container)!.value).toBe("/co");
    expect(mode(container)).toBe("INSERT");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(box(container)!, { key: "Escape" });
    expect(mode(container)).toBe("BROWSE");
    expect(postMessage.mock.calls.map(([msg]) => JSON.parse(msg).type)).not.toContain("interrupt");
  });
});

describe("Esc with a y/n question up", () => {
  it("answers the question: the menu closes with it and does not keep the Esc", () => {
    const { container } = inInput(undefined);
    type(container, "/co");
    expect(container.querySelector(".slash-menu")).not.toBeNull();
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    expect(container.querySelector(".slash-menu"), "the menu gives way to the question").toBeNull();
    // The Esc reaches the question (which cancels it); a menu that had stayed open would have swallowed it.
    fireEvent.keyDown(box(container)!, { key: "Escape" });
    expect(container.textContent ?? "").not.toContain("close 1? (y/n)");
    // The draft is untouched and the menu is back once the question is gone.
    expect(box(container)!.value).toBe("/co");
    expect(container.querySelector(".slash-menu")).not.toBeNull();
  });
});

/** The band lays itself out from its measured width, which jsdom never reports: this hands it one. */
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
        if (el === band) o.callback([{ target: el, contentRect: { width: 900 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}

describe("/rewind on a Claude tab", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("sends nothing, keeps the draft, and the panel shows its pointer once", () => {
    const widen = stubBandWidth();
    const posted: unknown[] = [];
    (window as unknown as { webkit: unknown }).webkit = {
      messageHandlers: { eitriAgent: { postMessage: (m: unknown) => posted.push(m) } },
    };
    const { container } = inInput(undefined);
    const pointer = "/rewind is not in Eitri — c reviews and reverts this turn's edits";
    type(container, "/rewind");
    const before = posted.length;
    fireEvent.keyDown(box(container)!, { key: "Enter" });
    expect(box(container)!.value).toBe("/rewind");
    act(() => widen(container));
    expect(container.querySelector(".status-band .band-message")?.textContent, "the band says it").toBe(pointer);
    expect(posted.slice(before).filter((m) => String(m).includes("/rewind")), "the draft never reaches the agent").toEqual([]);
    const shown = (container.textContent ?? "").split(pointer).length - 1;
    expect(shown, "the band and the composer must not both draw the pointer").toBe(1);
  });
});
