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

  it("the pane losing the keys and getting them back (a window focus round trip) leaves it open", () => {
    const rendered = started();
    opened(rendered);
    dispatch({ kind: "pane_focus", focused: false });
    expect(overlay(rendered.container)).not.toBeNull();
    dispatch({ kind: "pane_focus", focused: true });
    expect(overlay(rendered.container)).not.toBeNull();
    expect(overlay(rendered.container)!.textContent).toContain("core/src/x.rs");
    press(rendered.container, "q");
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
    expect(requests("open_in_editor")).toEqual([expect.objectContaining({ tab: 1, turn: 7, scope: "turn", path: "core/src/x.rs", line: 966 })]);
    expect(requests("open_path"), "the review's own message, not the conversation's").toEqual([]);
    press(container, "y");
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith("core/src/x.rs:966");
  });

  it("o on a file with no patch loaded opens it with no line", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "o");
    expect(requests("open_path")).toEqual([]);
    const sent = requests("open_in_editor")[0];
    expect(sent).toMatchObject({ tab: 1, turn: 7, scope: "turn", path: "core/src/x.rs" });
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

// ---------------------------------------------------------------------------------------------------------
// x, u, i, s, recovery and `o`'s answer: what the overlay posts, and what it says of the answers

const DRAFT = {
  comments: [{ id: 1, turn: 7, path: "core/src/x.rs", from: 837, to: 837, anchor: ["let b = 2;"], text: "why?" }],
  reverts: [{ id: 1, turn: 7, path: "core/src/x.rs", hunk: 1, header: "@@ -959,4 +965,9 @@", what: "hunk", lines: [966, 966], source: "panel", undone: false }],
  canUndo: true,
};

/** The overlay open on `core/src/x.rs` with its patch loaded and the cursor on the first hunk. */
function onHunk(rendered: ReturnType<typeof started>, over: Record<string, unknown> = {}) {
  opened(rendered, over);
  press(rendered.container, "Enter");
  dispatch(diffEnvelope(requests("review_diff_request")[0].request_id));
  press(rendered.container, "j");
  expect(overlay(rendered.container)!.querySelector(".row-current")!.getAttribute("data-nav-stop")).toBe("hunk");
}
const status = (c: HTMLElement) => overlay(c)?.querySelector(".review-status")?.textContent ?? null;
const commentBox = (c: HTMLElement) => overlay(c)?.querySelector<HTMLInputElement>(".review-prompt input") ?? null;

describe("x", () => {
  it("on a hunk posts the revert, naming the tab, turn, scope, path, hunk and header", () => {
    const rendered = started();
    onHunk(rendered);
    press(rendered.container, "x");
    expect(requests("review_revert")).toEqual([
      expect.objectContaining({ tab: 1, turn: 7, scope: "turn", path: "core/src/x.rs", target: { hunk: 0, header: "@@ -830,6 +830,12 @@" } }),
    ]);
  });

  it("on a file row asks, then y posts the whole-file revert and n posts nothing", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "x");
    expect(overlay(rendered.container)!.querySelector(".review-question")!.textContent).toBe("revert the whole file core/src/x.rs to before turn 7? y/n");
    expect(requests("review_revert")).toEqual([]);
    press(rendered.container, "n");
    expect(requests("review_revert")).toEqual([]);
    expect(overlay(rendered.container)!.querySelector(".review-question")).toBeNull();
    press(rendered.container, "x");
    press(rendered.container, "y");
    expect(requests("review_revert")).toEqual([expect.objectContaining({ tab: 1, turn: 7, path: "core/src/x.rs", target: "file" })]);
    expect(navigator.clipboard.writeText, "y answered the question, it did not copy").not.toHaveBeenCalled();
  });

  it("a refusal is the overlay's status line, in the shell's words", () => {
    const rendered = started();
    onHunk(rendered);
    press(rendered.container, "x");
    const request = requests("review_revert")[0];
    dispatch({ kind: "command_result", requestId: request.request_id, ok: false, error: "changed since the turn ended; open it in the editor (o)" });
    expect(status(rendered.container)).toBe("changed since the turn ended; open it in the editor (o)");
    expect(rendered.container.querySelector(".banner, .command-notice")).toBeNull();
    press(rendered.container, "j");
    expect(status(rendered.container), "the next key clears it").toBeNull();
  });

  it("on a comment posts its removal; the draft that comes back replaces the row", () => {
    const rendered = started();
    onHunk(rendered, { draft: DRAFT });
    press(rendered.container, "j");
    expect(overlay(rendered.container)!.querySelector(".row-current")!.getAttribute("data-nav-stop")).toBe("comment");
    press(rendered.container, "x");
    const request = requests("review_comment_remove")[0];
    expect(request).toMatchObject({ tab: 1, id: 1 });
    dispatch({ kind: "review_draft", requestId: request.request_id, tab: 1, draft: { comments: [], reverts: DRAFT.reverts, canUndo: true } });
    expect(overlay(rendered.container)!.querySelector('[data-nav-stop="comment"]')).toBeNull();
    expect(overlay(rendered.container)!.querySelector(".row-current")!.getAttribute("data-nav-stop"), "the cursor did not jump to the top").toBe("hunk");
  });
});

describe("u", () => {
  it("posts the undo when the draft can, and says there is nothing when it cannot", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "u");
    expect(requests("review_undo")).toEqual([]);
    expect(status(rendered.container)).toBe("nothing to undo");
    dispatch(reviewEnvelope(requests("review_request").pop()!.request_id, { draft: DRAFT }));
    press(rendered.container, "u");
    expect(requests("review_undo")).toEqual([expect.objectContaining({ tab: 1 })]);
  });

  it("the shell's word on a success is shown", () => {
    const rendered = started();
    opened(rendered, { draft: DRAFT });
    press(rendered.container, "u");
    dispatch({ kind: "command_result", requestId: requests("review_undo")[0].request_id, ok: true, message: "undone" });
    expect(status(rendered.container)).toBe("undone");
  });
});

describe("i", () => {
  it("opens a text box in the overlay, Enter posts the comment with the hunk's added lines and the user's own words", () => {
    const rendered = started();
    onHunk(rendered);
    press(rendered.container, "i");
    const box = commentBox(rendered.container)!;
    expect(box).not.toBeNull();
    expect(overlay(rendered.container)!.querySelector(".review-question")!.textContent).toBe("comment on core/src/x.rs:837");
    fireEvent.change(box, { target: { value: "why is b 2?" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(requests("review_comment_add")).toEqual([
      expect.objectContaining({ tab: 1, turn: 7, scope: "turn", path: "core/src/x.rs", from: 837, to: 837, text: "why is b 2?" }),
    ]);
    expect(commentBox(rendered.container)).toBeNull();
    // The comment shows once the shell sends the draft back for that request.
    const request = requests("review_comment_add")[0];
    dispatch({ kind: "review_draft", requestId: request.request_id, tab: 1, draft: DRAFT });
    expect(overlay(rendered.container)!.textContent).toContain("draft: 1 comment, 1 revert · s sends them to the agent");
    expect(overlay(rendered.container)!.querySelector('[data-nav-stop="comment"]')!.textContent).toContain("why?");
  });

  it("gets the keys back when the pane regains them with DOM focus lost (a window focus round trip)", () => {
    const rendered = started();
    onHunk(rendered);
    press(rendered.container, "i");
    const box = commentBox(rendered.container)!;
    expect(document.activeElement).toBe(box);
    dispatch({ kind: "pane_focus", focused: false });
    box.blur();
    expect(document.activeElement).not.toBe(box);
    dispatch({ kind: "pane_focus", focused: true });
    expect(commentBox(rendered.container)).toBe(box);
    expect(document.activeElement).toBe(box);
  });

  it("Escape closes the input and leaves the overlay open and the comment unsent", () => {
    const rendered = started();
    onHunk(rendered);
    press(rendered.container, "i");
    const box = commentBox(rendered.container)!;
    fireEvent.change(box, { target: { value: "never mind" } });
    fireEvent.keyDown(box, { key: "Escape" });
    expect(commentBox(rendered.container)).toBeNull();
    expect(overlay(rendered.container)).not.toBeNull();
    expect(requests("review_comment_add")).toEqual([]);
  });

  it("while it is open it owns the keys: a and d are letters in the box, and answer no card", () => {
    const rendered = started({ withCard: true });
    onHunk(rendered);
    press(rendered.container, "i");
    const box = commentBox(rendered.container)!;
    for (const k of ["a", "d", "D", "q", "c", "j", "y"]) {
      // `fireEvent` returns false when the default was prevented, which would stop the letter reaching the box.
      expect(fireEvent.keyDown(box, { key: k, ...(k === "D" ? { shiftKey: true } : {}) }), `${k} is typed, not claimed`).toBe(true);
      wait(TYPING_GUARD_MS + 50);
    }
    // Keys that reach the root while the box is open (focus was elsewhere) mean nothing either.
    for (const k of ["a", "d", "D", "x", "s", "u"]) {
      press(rendered.container, k, k === "D" ? { shiftKey: true } : {});
      wait(TYPING_GUARD_MS + 50);
    }
    expect(answered()).toEqual([]);
    expect(overlay(rendered.container)).not.toBeNull();
    expect(commentBox(rendered.container)).not.toBeNull();
    expect(requests("review_revert")).toEqual([]);
    expect(requests("review_send")).toEqual([]);
    // Escape reaching the root cancels the input and nothing else.
    press(rendered.container, "Escape");
    expect(commentBox(rendered.container)).toBeNull();
    expect(overlay(rendered.container)).not.toBeNull();
  });
});

describe("s", () => {
  it("says there is nothing to send for an empty draft", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "s");
    expect(requests("review_send")).toEqual([]);
    expect(status(rendered.container)).toBe("nothing to send");
  });

  it("asks for the preview, shows it, and y sends with its digest; the shell's word is the status", () => {
    const rendered = started();
    opened(rendered, { draft: DRAFT });
    press(rendered.container, "s");
    const ask = requests("review_send")[0];
    expect(ask).toMatchObject({ tab: 1, confirm: null });
    dispatch({
      kind: "review_send_preview",
      requestId: ask.request_id,
      tab: 1,
      digest: "9f2c4e1a0b7d3c55",
      text: "Review of your last turn: 1 comment, 1 revert.",
      notOnDisk: [{ id: 2, path: "core/src/y.rs", what: "hunk", lines: [10, 14], why: "only in the editor, not saved" }],
      queued: true,
    });
    const text = overlay(rendered.container)!.textContent!;
    expect(text).toContain("Review of your last turn: 1 comment, 1 revert.");
    expect(text).toContain("core/src/y.rs lines 10-14: only in the editor, not saved");
    expect(text).toContain("y queues it behind the running turn · n cancels");
    press(rendered.container, "y");
    const sent = requests("review_send")[1];
    expect(sent).toMatchObject({ tab: 1, confirm: "9f2c4e1a0b7d3c55" });
    dispatch({ kind: "command_result", requestId: sent.request_id, ok: true, message: "queued: it is sent when the running turn ends" });
    expect(status(rendered.container)).toBe("queued: it is sent when the running turn ends");
    expect(requests("review_send")).toHaveLength(2);
  });

  it("n sends nothing more, and a preview that arrives after it is ignored", () => {
    const rendered = started();
    opened(rendered, { draft: DRAFT });
    press(rendered.container, "s");
    const ask = requests("review_send")[0];
    press(rendered.container, "n");
    dispatch({ kind: "review_send_preview", requestId: ask.request_id, tab: 1, digest: "d", text: "late", notOnDisk: [], queued: false });
    expect(overlay(rendered.container)!.textContent).not.toContain("late");
    press(rendered.container, "y");
    expect(requests("review_send")).toHaveLength(1);
  });

  it("a draft pushed while the preview is up cancels it, so the digest read is the digest sent", () => {
    const rendered = started();
    opened(rendered, { draft: DRAFT });
    press(rendered.container, "s");
    dispatch({ kind: "review_send_preview", requestId: requests("review_send")[0].request_id, tab: 1, digest: "d", text: "msg", notOnDisk: [], queued: false });
    dispatch({ kind: "review_draft", requestId: null, tab: 1, draft: { ...DRAFT, comments: [] } });
    expect(overlay(rendered.container)!.querySelector(".review-preview")).toBeNull();
    press(rendered.container, "y");
    expect(requests("review_send")).toHaveLength(1);
  });

  it("a review_draft for a tab that is not on screen is dropped", () => {
    const rendered = started({ tabs: [TAB1, TAB2] });
    opened(rendered);
    dispatch({ kind: "review_draft", requestId: null, tab: 2, draft: DRAFT });
    expect(overlay(rendered.container)!.querySelector(".review-draft")).toBeNull();
  });

  it("the draft is drawn again from the overview after the page starts over", () => {
    const first = started();
    opened(first, { draft: DRAFT });
    expect(overlay(first.container)!.querySelector(".review-draft")!.textContent).toBe("draft: 1 comment, 1 revert · s sends them to the agent");
    cleanup();
    const second = started();
    press(second.container, "c");
    expect(overlay(second.container)!.querySelector(".review-draft"), "nothing was kept in the page").toBeNull();
    dispatch(reviewEnvelope(requests("review_request").pop()!.request_id, { draft: DRAFT }));
    expect(overlay(second.container)!.querySelector(".review-draft")!.textContent).toBe("draft: 1 comment, 1 revert · s sends them to the agent");
  });
});

describe("an interrupted revert", () => {
  const ENTRY = { id: "e-1", path: "core/src/x.rs", at: 1790000000000 };

  it("is named in the band with the pointer off, and gone when the shell says it is resolved", () => {
    const { container } = started();
    expect(bandText(container)).not.toContain("interrupted");
    dispatch({ kind: "review_recovery", entries: [ENTRY] });
    expect(bandText(container)).toContain("an interrupted revert left core/src/x.rs · c to review");
    expect(bandText(container)).not.toContain("files changed");
    dispatch({ kind: "review_recovery", entries: [] });
    expect(bandText(container)).not.toContain("interrupted");
  });

  it("is a window-level envelope: it shows for whichever tab is on screen", () => {
    const rendered = started({ tabs: [TAB1, TAB2] });
    dispatch({ kind: "review_recovery", entries: [ENTRY] });
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
    expect(bandText(rendered.container)).toContain("an interrupted revert left core/src/x.rs");
  });

  it("is a row at the top of the overlay; Enter asks, y restores, naming no tab", () => {
    const rendered = started();
    dispatch({ kind: "review_recovery", entries: [ENTRY] });
    opened(rendered);
    const first = overlay(rendered.container)!.querySelector(".row-current")!;
    expect(first.getAttribute("data-nav-stop")).toBe("recovery");
    press(rendered.container, "Enter");
    expect(overlay(rendered.container)!.querySelector(".review-question")!.textContent).toBe(
      "restore core/src/x.rs to its bytes from before the interrupted revert? the current bytes are kept in the review store. y restores · n forgets this",
    );
    press(rendered.container, "y");
    const sent = requests("review_recover")[0];
    expect(sent).toMatchObject({ entry: "e-1", answer: "restore" });
    expect(sent).not.toHaveProperty("tab");
  });

  it("n forgets it, and an entry that arrives while the overlay is open is drawn", () => {
    const rendered = started();
    opened(rendered);
    expect(overlay(rendered.container)!.querySelector('[data-nav-stop="recovery"]')).toBeNull();
    dispatch({ kind: "review_recovery", entries: [ENTRY] });
    press(rendered.container, "k");
    press(rendered.container, "Enter");
    press(rendered.container, "n");
    expect(requests("review_recover")).toEqual([expect.objectContaining({ entry: "e-1", answer: "dismiss" })]);
  });

  it("a refusal to restore is on the overlay's status line", () => {
    const rendered = started();
    dispatch({ kind: "review_recovery", entries: [ENTRY] });
    opened(rendered);
    press(rendered.container, "Enter");
    press(rendered.container, "y");
    dispatch({ kind: "command_result", requestId: requests("review_recover")[0].request_id, ok: false, error: "an agent turn is running" });
    expect(status(rendered.container)).toBe("an agent turn is running");
  });
});

describe("o", () => {
  it("a refusal from the editor is the overlay's status line, not a fading flash", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "o");
    dispatch({ kind: "command_result", requestId: requests("open_in_editor")[0].request_id, ok: false, error: "no editor is attached" });
    expect(status(rendered.container)).toBe("no editor is attached");
  });

  it("says what opened, in the shell's words", () => {
    const rendered = started();
    opened(rendered);
    press(rendered.container, "o");
    dispatch({ kind: "command_result", requestId: requests("open_in_editor")[0].request_id, ok: true, message: "opened; 2 hunks no longer match this buffer" });
    expect(status(rendered.container)).toBe("opened; 2 hunks no longer match this buffer");
  });
});
