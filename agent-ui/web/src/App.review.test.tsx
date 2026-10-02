// @vitest-environment jsdom
/**
 * The turn review overlay in the panel (`c` in BROWSE): how `App` opens it, what it asks the shell for, what it
 * owns while it is open, where it closes, and the status band's pointer to it. The overlay's own drawing is
 * `components/ReviewOverlay.test.tsx` and its keys and reducer `review.test.ts`; this file is the wiring.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

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
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;
const TAB2 = { ...TAB1, id: 2, number: 2, label: "2 new" } as const;

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
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const press = (c: HTMLElement, k: string, init: Record<string, unknown> = {}) => fireEvent.keyDown(root(c), { key: k, ...init });
const overlay = (c: HTMLElement) => c.querySelector<HTMLElement>(".review-overlay");
const requests = (type: string) => posted.filter((m) => m.type === type);
const answered = () => posted.filter((m) => m.type === "permission_response");
const bandText = (c: HTMLElement) => c.querySelector(".status-band")?.textContent ?? "";

/** A running turn in tab 1 with one card waiting, so a stray `a` would be seen answering it. */
function cardEvents(): AgentDomainEvent[] {
  return [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
    { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
    { type: "content_delta", turn_id: "t1", kind: "text", text: "meanwhile" },
  ];
}

/** Tab 1 live, the pane focused and the keys arrived, in BROWSE; `withCard` leaves a card waiting. */
function started({ withCard = false, tabs = [TAB1] as readonly Record<string, unknown>[] } = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  if (withCard) {
    const list = cardEvents();
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const T0 = Date.UTC(2026, 0, 1, 11, 0, 0);
const turn = (n: number, over: Record<string, unknown> = {}) => ({
  n, turnId: `t${n}`, startedAt: T0 + n * 1000, endedAt: T0 + n * 1000 + 500, state: "ok", late: false, overlappedNext: false, overlappedTab: false, reason: null, ...over,
});
const FILES = [
  { path: "core/src/x.rs", added: 41, removed: 6, origin: "agent", binary: false, tooLarge: false, nested: false },
  { path: "Cargo.lock", added: 7, removed: 7, origin: "workspace", binary: false, tooLarge: false, nested: false },
];
const reviewEnvelope = (requestId: unknown, over: Record<string, unknown> = {}) => ({
  kind: "review", requestId, tab: 1, scope: "turn", current: 7, turns: [turn(6), turn(7)], files: FILES,
  compared: true, pendingNoResult: 0, notes: ["changed on disk during this turn"], ...over,
});
const diffEnvelope = (requestId: unknown, over: Record<string, unknown> = {}) => ({
  kind: "review_diff", requestId, tab: 1, turn: 7, path: "core/src/x.rs", added: 41, removed: 6,
  hunks: [
    { id: 0, header: "@@ -830,6 +830,12 @@", lines: [
      { kind: "context", text: "let a = 1;", oldNo: 830, newNo: 830 },
      { kind: "added", text: "let b = 2;", oldNo: null, newNo: 837 },
    ] },
    { id: 1, header: "@@ -959,4 +965,9 @@", lines: [{ kind: "added", text: "later", oldNo: null, newNo: 966 }] },
  ],
  ...over,
});

/** `c`, and the shell's answer to the request it posted. */
function opened(rendered: ReturnType<typeof started>, over: Record<string, unknown> = {}) {
  press(rendered.container, "c");
  const request = requests("review_request").pop()!;
  dispatch(reviewEnvelope(request.request_id, over));
  return request;
}

describe("c opens the review overlay", () => {
  it("asks for the active tab's latest turn and shows the overlay, loading", () => {
    const { container } = started();
    press(container, "c");
    expect(requests("review_request")).toEqual([expect.objectContaining({ tab: 1, turn: "latest", scope: "turn" })]);
    expect(overlay(container)).not.toBeNull();
    expect(overlay(container)!.textContent).toContain("loading…");
  });

  it("fills in from the reply that answers its request, and takes no other", () => {
    const rendered = started();
    const { container } = rendered;
    press(container, "c");
    const request = requests("review_request")[0];
    dispatch(reviewEnvelope("someone-else"));
    expect(overlay(container)!.textContent).toContain("loading…");
    dispatch(reviewEnvelope(request.request_id));
    expect(overlay(container)!.querySelector(".review-title")!.textContent).toContain("review · turn 7 of 7");
    expect(overlay(container)!.textContent).toContain("core/src/x.rs");
    expect(overlay(container)!.textContent).toContain("changed outside this tab's edits (1)");
  });

  it("does nothing on the start screen, where there is no session", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB1, state: "not_started" }] });
    const target = container.querySelector<HTMLElement>(".agent-ui-root")!;
    fireEvent.keyDown(target, { key: "c" });
    fireEvent.keyDown(container.querySelector<HTMLElement>(".empty-tab") ?? target, { key: "c" });
    expect(requests("review_request")).toEqual([]);
    expect(overlay(container)).toBeNull();
  });

  it("is a letter in INPUT, not a command", () => {
    const { container } = started();
    press(container, "i");
    wait(TYPING_GUARD_MS + 50);
    const box = container.querySelector<HTMLTextAreaElement>(".composer textarea")!;
    fireEvent.keyDown(box, { key: "c" });
    expect(requests("review_request")).toEqual([]);
    expect(overlay(container)).toBeNull();
  });

  it("a refused request is said in the overlay's own header, not the conversation's banner", () => {
    const { container } = started();
    press(container, "c");
    const request = requests("review_request")[0];
    dispatch({ kind: "command_result", requestId: request.request_id, ok: false, error: "no session" });
    expect(overlay(container)!.querySelector(".review-header")!.textContent).toContain("no session");
    expect(overlay(container)!.textContent).not.toContain("loading…");
    expect(container.querySelector(".banner, .command-notice")).toBeNull();
  });
});

describe("closing", () => {
  it.each(["q", "Escape", "c"])("%s closes it", (k) => {
    const rendered = started();
    opened(rendered);
    expect(overlay(rendered.container)).not.toBeNull();
    press(rendered.container, k);
    expect(overlay(rendered.container)).toBeNull();
  });

  it.each([
    ["hint_collect", { kind: "hint_collect", sessionId: 1 }],
    ["pane_focus", { kind: "pane_focus", focused: false }],
    ["pane_focus (regained)", { kind: "pane_focus", focused: true }],
    ["enter_input", { kind: "enter_input" }],
    ["arrive", { kind: "arrive" }],
    ["open_keymap", { kind: "open_keymap" }],
  ])("%s closes it", (_name, envelope) => {
    const rendered = started();
    opened(rendered);
    expect(overlay(rendered.container)).not.toBeNull();
    dispatch(envelope);
    expect(overlay(rendered.container)).toBeNull();
  });

  it("another tab coming on screen closes it", () => {
    const rendered = started({ tabs: [TAB1, TAB2] });
    opened(rendered);
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    expect(overlay(rendered.container)).toBeNull();
  });

  it("a chooser opening over it closes it", () => {
    const rendered = started();
    opened(rendered);
    dispatch({ kind: "chooser", open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }], records: [] });
    expect(overlay(rendered.container)).toBeNull();
  });
});

describe("while it is open it owns every key", () => {
  it("a and d never reach the card underneath", () => {
    const rendered = started({ withCard: true });
    const { container } = rendered;
    opened(rendered);
    for (const k of ["a", "d", "D", "y", "Enter", "i", "f", "x", "u", "s"]) {
      press(container, k, k === "D" ? { shiftKey: true } : {});
      wait(TYPING_GUARD_MS + 50);
    }
    expect(answered()).toEqual([]);
    expect(requests("hint_request")).toEqual([]);
    expect(container.querySelector('[data-testid="mode-block"]')!.textContent).toBe("BROWSE");
  });

  it("the card is answered by a again once it is closed (the control for the test above)", () => {
    const rendered = started({ withCard: true });
    opened(rendered);
    press(rendered.container, "q");
    wait(TYPING_GUARD_MS + 50);
    press(rendered.container, "a");
    wait(TYPING_GUARD_MS + 50);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });

  it("j and k move over the stops, and the session's rows are untouched", () => {
    const rendered = started({ withCard: true });
    const { container } = rendered;
    const sessionRows = container.querySelectorAll('[data-nav-stop="row"]').length;
    const cursorBefore = container.querySelector(".message-list .row-current")?.textContent;
    opened(rendered);
    const stops = () => Array.from(overlay(container)!.querySelectorAll("[data-nav-stop]")).map((e) => e.getAttribute("data-nav-stop"));
    expect(stops()).toEqual(["file", "group"]);
    expect(overlay(container)!.querySelector(".row-current")!.textContent).toContain("core/src/x.rs");
    press(container, "j");
    expect(overlay(container)!.querySelector(".row-current")!.getAttribute("data-nav-stop")).toBe("group");
    press(container, "k");
    expect(overlay(container)!.querySelector(".row-current")!.getAttribute("data-nav-stop")).toBe("file");
    expect(container.querySelectorAll('[data-nav-stop="row"]')).toHaveLength(sessionRows);
    expect(container.querySelector(".message-list .row-current")?.textContent).toBe(cursorBefore);
  });
});

describe("Enter, o, y, [ ] and S", () => {
  it("Enter asks for the file's patch under the overlay's scope and draws the hunks it gets back", () => {
    const rendered = started();
    const { container } = rendered;
    opened(rendered);
    press(container, "Enter");
    const request = requests("review_diff_request")[0];
    expect(request).toMatchObject({ tab: 1, turn: 7, scope: "turn", path: "core/src/x.rs" });
    expect(overlay(container)!.querySelector(".review-patch")!.textContent).toBe("loading…");
    dispatch(diffEnvelope(request.request_id));
    expect(overlay(container)!.querySelectorAll('[data-nav-stop="hunk"]')).toHaveLength(2);
    expect(overlay(container)!.querySelectorAll(".diff-added")).toHaveLength(2);
    expect(overlay(container)!.textContent).toContain("@@ -830,6 +830,12 @@");
  });

  it("a patch the shell would not send is refused, with its counts", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "Enter");
    dispatch(diffEnvelope(requests("review_diff_request")[0].request_id, { hunks: null }));
    expect(overlay(rendered.container)!.textContent).toContain("too large (+41 −6 lines); o opens it in the editor");
    expect(overlay(rendered.container)!.querySelector(".review-hunk")).toBeNull();
  });

  it("o on a hunk opens the file at the hunk's first new line, y copies path:line", () => {
    const rendered = started();
    const { container } = rendered;
    opened(rendered);
    press(container, "Enter");
    dispatch(diffEnvelope(requests("review_diff_request")[0].request_id));
    press(container, "j");
    press(container, "j");
    expect(overlay(container)!.querySelector(".row-current")!.textContent).toContain("@@ -959,4 +965,9 @@");
    press(container, "o");
    expect(requests("open_path")).toEqual([expect.objectContaining({ path: "core/src/x.rs", line: 966 })]);
    press(container, "y");
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith("core/src/x.rs:966");
  });

  it("o on a file with no patch loaded opens it with no line", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "o");
    const sent = requests("open_path")[0];
    expect(sent).toMatchObject({ path: "core/src/x.rs" });
    expect(sent).not.toHaveProperty("line");
  });

  it("[ asks for the turn before, S for the whole session; each names the tab", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "[");
    const back = requests("review_request").pop()!;
    expect(back).toMatchObject({ tab: 1, turn: 6, scope: "turn" });
    dispatch(reviewEnvelope(back.request_id, { current: 6 }));
    press(rendered.container, "S", { shiftKey: true });
    expect(requests("review_request").pop()).toMatchObject({ tab: 1, scope: "session" });
  });

  it("a reply to a request the overlay has replaced is dropped", () => {
    const rendered = started();
    const first = opened(rendered);
    press(rendered.container, "[");
    dispatch(reviewEnvelope(first.request_id, { current: 7 }));
    expect(overlay(rendered.container)!.textContent).not.toContain("turn 6");
    const second = requests("review_request").pop()!;
    dispatch(reviewEnvelope(second.request_id, { current: 6 }));
    expect(overlay(rendered.container)!.querySelector(".review-title")!.textContent).toContain("turn 6 of 7");
  });

  it("the keys x, u, i and s do nothing yet", () => {
    const rendered = started();
    opened(rendered);
    const before = posted.length;
    for (const k of ["x", "u", "i", "s"]) press(rendered.container, k);
    expect(posted.length).toBe(before);
    expect(overlay(rendered.container)).not.toBeNull();
  });
});

describe("the status band's pointer", () => {
  it("appears for the tab it names and says how many files changed", () => {
    const { container } = started();
    expect(bandText(container)).not.toContain("to review");
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    expect(bandText(container)).toContain("3 files changed · c to review");
    dispatch({ kind: "review_hint", tab: 1, turn: 8, files: 1 });
    expect(bandText(container)).toContain("1 file changed · c to review");
  });

  it("files: 0 clears it", () => {
    const { container } = started();
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 0 });
    expect(bandText(container)).not.toContain("to review");
  });

  it("is kept for a tab that is not on screen, and shown when it comes", () => {
    const rendered = started({ tabs: [TAB1, TAB2] });
    dispatch({ kind: "review_hint", tab: 2, turn: 4, files: 2 });
    expect(bandText(rendered.container)).not.toContain("to review");
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
    expect(bandText(rendered.container)).toContain("2 files changed · c to review");
  });

  it("goes once the overlay is opened on that turn", () => {
    const rendered = started();
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    opened(rendered, { current: 7 });
    expect(bandText(rendered.container)).not.toContain("to review");
  });

  it("stays when the overlay is opened on an earlier turn", () => {
    const rendered = started();
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    opened(rendered, { current: 6 });
    expect(bandText(rendered.container)).toContain("3 files changed · c to review");
  });

  it("goes when a new turn starts", () => {
    const { container } = started();
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    const events: AgentDomainEvent[] = [{ type: "turn_started", turn_id: "t9" }];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events });
    expect(bandText(container)).not.toContain("to review");
  });

  it("is not shown for a turn that is already over by the time it arrives, while another runs", () => {
    const { container } = started();
    const events: AgentDomainEvent[] = [{ type: "turn_started", turn_id: "t9" }];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events });
    dispatch({ kind: "review_hint", tab: 1, turn: 7, files: 3 });
    expect(bandText(container)).not.toContain("to review");
  });
});
