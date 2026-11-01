// @vitest-environment jsdom
/**
 * The Codex cards and rows in the whole panel: `D` on a card that has no reason box says so, and the rows of a
 * Codex turn name their subagent. The payloads come from `fixtures/codex-cards.json`, which Rust writes from the
 * provider's own constructors. What each card shows is `components/PermissionCard.codex.test.tsx`.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello } from "./types";
import fixture from "./fixtures/codex-cards.json";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers();
  widen = stubBandWidth();
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn() }, configurable: true });
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

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 codex", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null, agent: "codex",
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
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
let widen: (container: HTMLElement) => void;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));

/** Tab 1 live, the pane focused and the keys arrived, in BROWSE. */
function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const caseOf = (name: string) => fixture.cases.find((c) => c.name === name)!;
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;

describe("a Codex tab", () => {
  it("reason box absent and D flashes", () => {
    const { container } = started();
    dispatch({ ...caseOf("command").snapshot, tab: 1 });
    fireEvent.keyDown(root(container), { key: "j" });
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    expect(container.querySelector(".permission-card input")).toBeNull();
    wait(TYPING_GUARD_MS + 50);
    posted.length = 0;
    fireEvent.keyDown(root(container), { key: "D", shiftKey: true });
    wait(TYPING_GUARD_MS + 50);
    expect(container.querySelector(".band-message")!.textContent).toBe("Codex takes no reason with a deny; d denies");
    // Nothing was answered, and nothing took the keys into a box.
    expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
    expect(document.activeElement?.tagName).not.toBe("INPUT");
  });

  it("codex rows render with subagent labels", () => {
    const { container } = started();
    dispatch({ ...caseOf("rows").snapshot, tab: 1 });
    const text = container.textContent!;
    // The tab's own command has no label, the subagent's has.
    const notes = Array.from(container.querySelectorAll(".tool-subagent-note")).map((el) => el.textContent);
    expect(notes).toEqual(["subagent /root/builder"]);
    expect(text).toContain("$ /bin/bash -lc 'ls'");
    expect(text).toContain("$ /bin/bash -lc 'cargo build'");
    expect(text).toContain("/project/a.txt");
    expect(text).toContain("docs · lookup");
    expect(text).toContain("Search: tokio runtime");
    expect(text).toContain("Subagent spawnAgent: Run the tests");
    // A file change is a row of its own, never counted into a run of other calls.
    expect(container.querySelector('[data-tool-name="fileChange"]')).not.toBeNull();
    expect(container.querySelector(".tool-card-run")).toBeNull();
  });

  it("the latest usage limit ending names when the window resets", () => {
    const { container } = started();
    dispatch({ ...caseOf("usage_limit").snapshot, tab: 1 });
    const row = container.querySelector(".row-ending .row-body")!.textContent!;
    expect(row).toMatch(/^stopped at the usage limit \(resets .*\d\d:\d\d\): You've hit your usage limit\.$/);
    expect(container.querySelector(".status-band .band-ending")!.textContent).toBe("limit reached");
  });
});
