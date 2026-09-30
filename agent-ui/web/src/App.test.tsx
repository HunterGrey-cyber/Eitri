// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App, { accumulateMotionCount, HINT_PENDING_TIMEOUT_MS, jumpTarget, MAX_MOTION_COUNT, statusWarning } from "./App";
import { applyEvent, initialState } from "./reducer";
import { RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT } from "./follow";
import type { AgentDomainEvent, AgentUiState, Hello, ProviderInfo, UsageInfo } from "./types";
import { WHICH_KEY_DELAY_MS } from "./leader";
import { EMPTY_PANEL_TABLE } from "./keymap";
import type { PanelTable } from "./keymap";
// Fix round 2 (v1 audit review, mutation resistance): a namespace import so `h3b` below can spy on
// the real `isPlainAnswerKey` export and prove the bypass-y handler actually calls it, rather than
// a same-shaped hand-written copy -- see that test's own comment for what this can and cannot catch.
import * as keymapModule from "./keymap";
import { binding, TABLE } from "./testFixtures";
import { modeFixedMessage } from "./modeKey";
import { hintTypingFlash, tableKeyTypingFlash, TYPING_GUARD_MS } from "./typingGuard";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so MessageList's auto-scroll would throw on a missing method.
  Element.prototype.scrollIntoView = vi.fn();
});

/** Everything the page posts to Rust, in order. `postToRust` reads this exact path on `window`, so
 *  installing a handler here exercises the real bridge module rather than a mocked one. */
let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { neovibeAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});

/** Delivers one envelope the way `agent_panel.rs`'s `evaluate_js_dispatch` does: a JSON string into
 *  the global the page installed on mount. */
function dispatch(payload: unknown) {
  act(() => {
    window.__neovibeDispatch!(JSON.stringify(payload));
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

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** Tab 1, live, resumable false (legacy has no resume): what almost every test below wants once a
 *  session is running. Session tabs (Task 9): the panel now learns a tab is live from a `tabs`
 *  envelope, not from `snapshot` alone. */
const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

/** What Rust sends for a window whose tab 1 holds a live session: `tabs`, then the snapshot. */
function dispatchLiveTab(state: AgentUiState, throughRevision = 1) {
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision, state });
}

/** An empty tab 1, which is how every window starts. */
function dispatchEmptyTab() {
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
}

function lastOfType(type: string): Record<string, unknown> | undefined {
  // No `.at(-1)`: tsconfig targets ES2020, and `cargo build -p shell` type-checks this file as part
  // of its own build, so an ES2022 method here fails the Rust build rather than just this test.
  const matching = posted.filter((m) => m.type === type);
  return matching[matching.length - 1];
}

/** The panel now opens in BROWSE, where the composer renders a hint line rather than a textarea
 *  (Task 5). Every test below that types into the box goes through the same `i` the real keyboard
 *  table (`./keymap`) offers, rather than reaching for the textarea directly -- which is also what
 *  proves the wiring in `App.tsx`'s `onKeyDown` actually works, not just `resolveKey` in isolation. */
function enterInputMode(container: HTMLElement) {
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "i" });
}

/** `navigator.clipboard` does not exist in jsdom at all -- both the conversation's `y` handler and
 *  the start screen's `handleStartScreenKeyDown` read it with `?.`. Stub-then-remove rather than a
 *  plain assignment, so a real environment gap here does not turn into cross-test pollution; the
 *  module-level `afterEach` below always cleans it up, whether or not a given test stubbed it. */
function stubClipboard(): { writeText: ReturnType<typeof vi.fn> } {
  const clipboard = { writeText: vi.fn() };
  Object.defineProperty(navigator, "clipboard", { value: clipboard, configurable: true });
  return clipboard;
}
afterEach(() => {
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});

/** Types `text` a character at a time via real keydown, appending each character only if that
 *  keydown's default was NOT prevented -- the same thing a real browser does before it inserts a
 *  character into a focused text field. This is what makes a test like "typing `j` into a
 *  permission card's reason box while in BROWSE gives the whole word" mean something: `fireEvent`
 *  does not simulate real text insertion at all, so asserting on `.value` after a bare
 *  `fireEvent.keyDown` would pass whether or not the panel's own handler swallowed the key. */
function typeIntoInput(input: HTMLInputElement, text: string) {
  for (const ch of text) {
    const notPrevented = fireEvent.keyDown(input, { key: ch });
    if (notPrevented) fireEvent.change(input, { target: { value: input.value + ch } });
  }
}

/** `gg`: the cursor to the first row. A snapshot now lands the cursor on the last row, where the
 *  view is (the phase-3 GUI pass, 2026-09-25); tests written for a cursor starting at the top say so. */
function gg(container: HTMLElement) {
  const root = container.querySelector(".agent-ui-conversation")!;
  fireEvent.keyDown(root, { key: "g" });
  fireEvent.keyDown(root, { key: "g" });
}

function buttonLabelled(container: HTMLElement, label: string): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(label));
}

/** Stubs `ResizeObserver` so the band can be widened past `band.ts`'s own pre-measurement floor
 *  (Review Focus 3: mode and pill only, until a real width is known) -- panel round 2 plan Task 10.
 *  Must be called BEFORE the render that mounts a `StatusBand`: its `useLayoutEffect` only creates a
 *  real observer if one exists at mount time, the same requirement `MessageList.test.tsx`'s own
 *  `stubResizeObserver` documents. Call the returned function with the rendered `container` once,
 *  any time after render, to report a wide band and a plausible mono character width; call
 *  `vi.unstubAllGlobals()` afterwards so the stub does not leak into `MessageList`'s own unrelated
 *  use of the same global in a later test. */
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

/** Opens `ContinueInTerminal`'s confirmation the way a real session does now (panel round 2 plan,
 *  Task 10): a click on the band opens the detail popover, `tab_detail` answers it, and a click on
 *  its trailing row opens the confirmation -- there is no permanently visible button to click
 *  directly any more (`<leader>t` is the other entry point, exercised by its own leader tests
 *  elsewhere). The band click works at any width (mode/pill's own floor, `band.ts`'s
 *  pre-measurement rule, never drops that button, so this needs no `stubBandWidth`). `rows` lets a
 *  caller shape the popover's own facts when a test cares; empty is fine when only the trailing row
 *  matters. */
function openHandoffConfirm(container: HTMLElement, tab = 1, rows: { label: string; value: string }[] = []) {
  fireEvent.click(container.querySelector(".band-open")!);
  dispatch({ kind: "tab_detail", tab, rows });
  const row = Array.from(container.querySelectorAll(".detail-popover tr")).find((tr) => tr.textContent?.includes("Continue in a terminal"))!;
  fireEvent.click(row);
}

describe("pane focus", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }

  it("draws the mode block dim until shell says this pane has focus, and follows it both ways", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    // shell focuses the editor at startup, so the panel claims nothing until told otherwise.
    expect(modeBlock(container).dataset.focused).toBe("false");
    dispatch({ kind: "pane_focus", focused: true });
    expect(modeBlock(container).dataset.focused).toBe("true");
    expect(modeBlock(container).dataset.mode).toBe("browse");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).dataset.focused).toBe("false");
    // The mode itself is untouched (focus is a separate fact from which mode the panel is in), but
    // the band names it only while the panel holds the keys (v1 polish F24).
    expect(modeBlock(container).dataset.mode).toBe("browse");
    expect(modeBlock(container).textContent).toBe("");
  });

  /** v1 polish F24: the empty tab starts in INPUT at launch while the editor holds the keys; its
   *  band read `INPUT`. It names a mode only once the panel has the keys, as the live tab's does. */
  it("the empty tab's band names no mode while the keys are elsewhere", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    expect(modeBlock(container).dataset.focused).toBe("false");
    expect(modeBlock(container).textContent).toBe("");
    dispatch({ kind: "pane_focus", focused: true });
    expect(modeBlock(container).dataset.mode).toBe("input");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).textContent).toBe("");
  });

  it("opens the composer with the caret in it when shell says the user arrived by keyboard", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    expect(modeBlock(container).dataset.mode).toBe("input");
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    expect(container.textContent).not.toContain("按 i 开始输入");
  });

  it("does not open the composer on a session that has ended, the same as i", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({
      kind: "events", tab: 1,
      fromRevision: 1,
      throughRevision: 2,
      events: [{ type: "session_closed", reason: "provider exited" }],
    });
    dispatch({ kind: "enter_input" });
    expect(modeBlock(container).dataset.mode).toBe("browse");
  });

  it("does not change the mode or what i does", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    enterInputMode(container);
    expect(modeBlock(container).dataset.mode).toBe("input");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).dataset.mode).toBe("input");
    expect(modeBlock(container).textContent).toBe("");
    expect(modeBlock(container).dataset.focused).toBe("false");
  });
});

/** Panel round 2 (spec §8, decision 4): reverses the 2026-09-19 ruling "control l直接闪cursor" for
 *  every keyboard arrival except a brand-new tab's (`enter_input`, unchanged -- see the "pane focus"
 *  describe above). A card waiting still lands as P1 always has; see the "P1: the keys land on a
 *  card that waits" describe below for that half. */
describe("arrive (panel round 2, spec §8, decision 4)", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }

  /* #22 (owner decision, 2026-09-29) reversed this describe's first test, which moved the cursor away
     with `gg` and asserted the arrival landed on the last row anyway. Split: a reader on the last row
     (following) still lands there, following resumed; one who put the cursor elsewhere keeps it. The
     park-and-restore cases are the "#22: coming back restores where you were" describe below. */
  it("with no card, from INPUT on the last row: BROWSE on the last row, following resumed, no textarea focused", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 2);
    dispatch({ kind: "pane_focus", focused: true });
    expect(container.querySelector(".row-current")!.textContent).toContain("second");
    enterInputMode(container);
    expect(modeBlock(container).dataset.mode).toBe("input");
    const seen: string[] = [];
    const onResume = () => seen.push("resume");
    document.addEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    try {
      dispatch({ kind: "arrive" });
    } finally {
      document.removeEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    }
    expect(modeBlock(container).dataset.mode).toBe("browse");
    expect(container.querySelector(".row-current")!.textContent).toContain("second");
    expect(seen).toEqual(["resume"]);
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("with no card, the cursor moved off the last row: BROWSE where it is, following not resumed (#22)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 2);
    dispatch({ kind: "pane_focus", focused: true });
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(container.querySelector(".row-current")!.textContent).toContain("first");
    enterInputMode(container);
    const seen: string[] = [];
    const onResume = () => seen.push("resume");
    document.addEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    try {
      dispatch({ kind: "arrive" });
    } finally {
      document.removeEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    }
    expect(modeBlock(container).dataset.mode).toBe("browse");
    expect(container.querySelector(".row-current")!.textContent).toContain("first");
    expect(seen).toEqual([]);
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("on an empty tab: BROWSE, no textarea focused (the dashboard cursor is Task 12's)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    expect(container.querySelector("textarea")).not.toBeNull();
    dispatch({ kind: "arrive" });
    expect(container.querySelector("textarea")).toBeNull();
  });

  /** GUI pass (2026-09-26, r2-gui): the empty tab's band read `INPUT` whatever the tab was in --
   *  its facts hard-coded `mode: "input"`, so an arrival that put the dashboard in BROWSE still
   *  said INPUT under it. */
  it("on an empty tab, the band shows the tab's own mode", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    expect(modeBlock(container).dataset.mode).toBe("input");
    dispatch({ kind: "arrive" });
    expect(modeBlock(container).dataset.mode).toBe("browse");
  });

  /** GUI pass (2026-09-26, r2-gui), R7: `EmptyTab` read the window's `arrive` counter as a new
   *  request whenever it MOUNTED, so once any keyboard arrival had happened, every new tab made from
   *  a live one (`prefix c`, `<leader>fn`, the chooser's New session) mounted, saw a stale count and
   *  dropped straight to BROWSE -- with nothing focused, so the keys were dead. */
  it("a new tab made from a live tab after an earlier arrival still lands INPUT (R7)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "only" }] }));
    dispatch({ kind: "arrive" });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
    // One batch, as WebKitGTK delivered them in the pass: Rust's `new_tab()` and `enter_input()`
    // run back to back, and React committed both before the new `EmptyTab`'s mount effects ran.
    act(() => {
      window.__neovibeDispatch!(JSON.stringify({ kind: "tabs", active: 2, tabs: two }));
      window.__neovibeDispatch!(JSON.stringify({ kind: "enter_input" }));
    });
    const box = container.querySelector("textarea");
    expect(box).not.toBeNull();
    expect(document.activeElement).toBe(box);
    expect(modeBlock(container).dataset.mode).toBe("input");
  });

  /** Decision 4: a tab switch lands BROWSE -- onto an empty tab too, where BROWSE is the dashboard
   *  (spec §8, "On an empty tab, the New session item"). */
  it("a switch to an existing empty tab lands BROWSE, not a composer nobody asked for", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "only" }] }) });
    dispatch({ kind: "tabs", active: 2, tabs: two });
    expect(container.querySelector("textarea")).toBeNull();
    expect(modeBlock(container).dataset.mode).toBe("browse");
  });

  /** Integration of wave 3's focus route with r2-gui's edge-only requests: the empty tab ignores
   *  request counts reached before it mounted, so a switch onto it from a live tab (whose root just
   *  unmounted, taking focus with it) must land the keys on the dashboard itself. */
  it("a switch from a live tab onto an empty tab leaves the keys on the dashboard", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "only" }] }) });
    act(() => container.querySelector<HTMLElement>(".agent-ui-root")!.focus());
    dispatch({ kind: "tabs", active: 2, tabs: two });
    expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
  });

  /** Owner answers Q1: a tab switch itself (not `arrive`) also lands BROWSE now, keeping `ca317ff`'s
   *  cursor/scroll restore exactly -- this is the switch's own reset (`tabs`/`snapshot`), not the
   *  `arrive` envelope. The `ca317ff` tests ("keeps each tab's cursor across a switch" and the small-
   *  defects/phase-3 GUI-pass describes below) are otherwise unchanged. */
  it("a tabs envelope switching the active tab while in INPUT lands BROWSE", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "only" }] }) });
    enterInputMode(container);
    expect(modeBlock(container).dataset.mode).toBe("input");
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    expect(modeBlock(container).dataset.mode).toBe("browse");
  });
});

describe("focus_permission (the tray's agent chip, or Ctrl+a a, with a card waiting)", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }
  function withTwoCards() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_2", name: "Write", input: { file_path: "a" } },
      { type: "permission_requested", permission_id: "perm-2", tool_use_id: "toolu_2", tool_name: "Write", input: {} },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    return rendered;
  }

  it("lands in BROWSE on the oldest pending card", () => {
    const { container } = withTwoCards();
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(modeBlock(container).dataset.mode).toBe("browse");
    const current = container.querySelector(".row-current")!;
    expect(current.classList.contains("row-permission")).toBe(true);
    expect(current.textContent).toContain("Permission requested: Bash");
  });

  it("leaves INPUT for the card, so a lone a answers it (after v1 S1's wait)", () => {
    vi.useFakeTimers();
    try {
      const { container } = withTwoCards();
      enterInputMode(container);
      expect(modeBlock(container).dataset.mode).toBe("input");
      dispatch({ kind: "focus_permission", tab: 1 });
      expect(modeBlock(container).dataset.mode).toBe("browse");
      // The `i` above is a key: a lone `a` is one nothing came near (spec 2026-09-27 §2.1).
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
      act(() => vi.advanceTimersByTime(250));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("takes the composer when the card was answered in between", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(modeBlock(container).dataset.mode).toBe("input");
  });

  // Fix round 1 (reviewer finding): the "no card to land on, treat as an ordinary arrival" fallback
  // above carries the same caret rule `i`/`o` do -- "kept" -- rather than whatever `composerCaret`
  // was last left at by an earlier `A` press (that press bumps `composerFocusRequest` directly,
  // never `inputRequest`, so nothing about it should leak into a later arrival here). Reproduced
  // without this fix: the caret below landed at the end of the draft, not at 3.
  it("takes the composer with the caret kept, not wherever an earlier A left composerCaret", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    const conversationRoot = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(conversationRoot, { key: "A", shiftKey: true }); // composerCaret -> "end", once.
    const box1 = container.querySelector("textarea")!;
    fireEvent.change(box1, { target: { value: "hello world" } });
    box1.setSelectionRange(3, 3);
    fireEvent.keyUp(box1, { key: "ArrowLeft" });
    fireEvent.keyDown(box1, { key: "Escape" });
    // No pending card by the time this arrives -- the "ordinary arrival" fallback, never i/o/A again.
    dispatch({ kind: "focus_permission", tab: 1 });
    const box2 = container.querySelector("textarea")!;
    expect(box2.selectionStart).toBe(3);
    expect(box2.selectionEnd).toBe(3);
  });

  /* The merge of modules P2 with the streaming-scroll fix (2026-09-24): landing on the card moves the
     cursor, and the `[cursor]` effect reveals its row -- a scroll the user asked for, like a HINT
     landing's. Unannounced, `MessageList` takes it for nobody's and keeps following, and the next
     delta or resize snaps an older card straight back out of view. */
  it("announces the landing to the message list, so revealing an older card is the user's scroll", () => {
    const { container } = withTwoCards();
    const list = container.querySelector(".message-list")!;
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(seen).toEqual(["unknown"]);
  });
});

describe("literal_key C-a (send-prefix from shell's prefix)", () => {
  it("selects all of the composer textarea's text while it has focus", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    enterInputMode(container);
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello world" } });
    dispatch({ kind: "literal_key", key: "C-a" });
    expect(textarea.selectionStart).toBe(0);
    expect(textarea.selectionEnd).toBe(textarea.value.length);
  });

  it("does nothing in BROWSE, where no text field has focus", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    const before = document.activeElement;
    expect(() => dispatch({ kind: "literal_key", key: "C-a" })).not.toThrow();
    expect(document.activeElement).toBe(before);
  });

  it("ignores a literal key other than C-a", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    enterInputMode(container);
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "hello" } });
    box.setSelectionRange(5, 5);
    act(() => dispatch({ kind: "literal_key", key: "C-b" }));
    expect(box.selectionStart).toBe(5);
    expect(box.selectionEnd).toBe(5);
  });
});

describe("App handshake", () => {
  it("announces itself with a `ready` carrying a request id, before anything else", () => {
    render(<App />);
    // V1 C1 (spec §3.5): `panel_keys` now follows on mount too (the mirror's own "once after ready"),
    // so this no longer asserts the whole post list is length 1 -- only that `ready` leads it. See
    // "the composer mirror" describe block below for what that second post actually says.
    expect(posted[0].type).toBe("ready");
    expect(typeof posted[0].request_id).toBe("string");
    // Fix round 1 (reviewer finding): pin what the second post actually is, rather than leaving the
    // comment above as the only thing saying so -- before `hello`/`tabs`, `activeTab` is `null` and
    // there is no box of any kind, so the mirror's first value is `other`.
    expect(posted[1]).toMatchObject({ type: "panel_keys", mode: "other" });
  });

  it("waits for a tabs envelope before showing anything but a connecting message", () => {
    const { container } = render(<App />);
    expect(container.textContent).toContain("Connecting to the shell");
    dispatch({ kind: "hello", ...HELLO });
    // `hello` alone names no tab yet -- the empty tab (F3) needs `activeTab`, from `tabs`.
    expect(container.textContent).toContain("Connecting to the shell");
    dispatchEmptyTab();
    expect(container.querySelector("textarea")).not.toBeNull();
  });

  it("sends what is typed, which starts the session lazily in Rust (ruling 4), and shows it is connecting once the tab says so", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "fix the bug" } });
    fireEvent.keyDown(box, { key: "Enter" });
    // There is no `start_session` message any more (ruling 4): the tab's mode and the fresh turn's
    // text travel on `send_message` alone, named by tab.
    expect(lastOfType("start_session")).toBeUndefined();
    expect(lastOfType("send_message")).toMatchObject({ tab: 1, text: "fix the bug" });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    expect(container.textContent).toContain("Starting the agent backend");
  });

  it("shows the tab's own permission mode in the band's mode pill once it is live", () => {
    // AgentUiState carries no field for this: the mode lives on the TAB now (spec §3.1), not
    // remembered here from a click -- there is no button to click any more. The winbar is gone
    // (V2, session tabs Task 10); the pill is the band's now (panel round 2 plan, Task 10), always
    // in its short form (`⏵⏵ <mode>`, no "on"/cycle hint -- those moved to `prefix i`), and
    // `started: true` here since this is the session-started render.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, mode: "bypass" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
    expect(container.querySelector(".status-band .mode-pill")?.textContent).toBe("⏵⏵ bypass permissions on");
  });

  it("shows why the tab failed to start, from the tabs envelope alone", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({
      kind: "tabs",
      active: 1,
      tabs: [{ ...LIVE_TAB, state: "failed", failure: "claude is not on PATH" }],
    });
    expect(container.textContent).toContain("claude is not on PATH");
    expect(container.textContent).not.toContain("Starting the agent backend");
  });
});

/* The reload path, from the frontend's side: a fresh document mounts, sends `ready`, and Rust
   answers with hello + a snapshot of the session that never stopped running. The conversation must
   come back from that snapshot alone -- with no flash of the start screen, and without this page
   ever having seen the events that built it. */
describe("App rehydration from a snapshot", () => {
  it("shows the conversation directly on a snapshot, without a mode selector in between", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({ model: "claude-opus-5", transcript: [{ seq: 0, text: "pre-reload marker alpha seven." }] }),
      7,
    );
    expect(container.querySelector(".agent-ui-conversation")).not.toBeNull();
    expect(container.querySelector(".empty-tab")).toBeNull();
    expect(container.textContent).toContain("pre-reload marker alpha seven.");
  });

  /* The phase-3 GUI pass (2026-09-25): after `prefix r` the view was at the bottom (MessageList
     follows from mount) but the cursor sat on row 1, off screen. A reload's first snapshot puts the
     cursor on the last row, where the view is -- as a conversation that grew in this page does. */
  it("puts the cursor on the last row after a reload's snapshot, where the view is", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        userPrompts: [{ seq: 0, text: "first question" }],
        transcript: [
          { seq: 1, text: "first answer" },
          { seq: 3, text: "last answer" },
        ],
        toolCalls: [],
      }),
      7,
    );
    const current = container.querySelector(".row-current");
    expect(current?.textContent).toContain("last answer");
  });

  it("ignores a command_result for a request this document never sent", () => {
    // Exactly what a reload produces: the reply to the pre-reload page's `start_session` arrives at
    // a page that has no record of it. It must not be read as this page's own start failing.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 0, text: "still here" }] }), 1);
    dispatch({ kind: "command_result", requestId: "req-from-a-previous-page", ok: true });
    expect(container.textContent).toContain("still here");
    expect(container.querySelector(".empty-tab")).toBeNull();
  });

  /* V3's other half (spec §4.2): the WebView holds only the active tab's state, so a switch never
   * renders a stale conversation underneath the new one. */
  it("renders only the active tab's conversation (V3)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state: snapshotState({ transcript: [{ seq: 1, text: "tab one" }] }) });
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 2, state: snapshotState({ transcript: [{ seq: 1, text: "tab two" }] }) });
    expect(container.textContent).toContain("tab two");
    expect(container.textContent).not.toContain("tab one");
  });
});

describe("App event folding", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    enterInputMode(rendered.container);
    return rendered;
  }

  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }

  it("accumulates streamed text into one assistant message, as the reducer does", () => {
    const { container } = startedApp();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "Hello " },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "world" },
    );
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(1);
    expect(container.querySelector(".row-assistant")!.textContent).toContain("Hello world");
  });

  it("keeps the composer live for a whole turn and queues what is typed (C1)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    enterInputMode(container);
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 1,
      throughRevision: 2,
      events: [{ type: "turn_started", turn_id: "t1" }],
    });
    const box = container.querySelector("textarea")!;
    expect(box.disabled).toBe(false);
    fireEvent.change(box, { target: { value: "and the tests" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(lastOfType("queue_message")).toMatchObject({ tab: 1, text: "and the tests" });
    expect(lastOfType("send_message")).toBeUndefined();
  });

  it("does not strand enter_input during a running turn (F1)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    expect(container.querySelector<HTMLTextAreaElement>("textarea")!.disabled).toBe(false);
  });

  it("sends a typed turn and clears the box", () => {
    const { container } = startedApp();
    fireEvent.change(container.querySelector("textarea")!, { target: { value: "what number?" } });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
    expect(lastOfType("send_message")!.text).toBe("what number?");
  });

  it("relays a permission decision with the id of the card that was clicked", () => {
    const { container } = startedApp();
    events({ type: "permission_requested", permission_id: "perm-9", tool_use_id: "toolu_9", tool_name: "Bash", input: {} });
    fireEvent.click(buttonLabelled(container, "Approve")!);
    const response = lastOfType("permission_response")!;
    expect(response.permission_id).toBe("perm-9");
    expect(response.decision).toBe("allow");
  });

  /* A session that died must say so, and must stop offering turns -- an enabled composer pointed at
     nothing is worse than a disabled one. */
  it("announces a lost session and warns the transcript may be incomplete", () => {
    const { container } = startedApp();
    events({ type: "session_unavailable", reason: "provider process exited unexpectedly" });
    const row = container.querySelector(".row-error")!;
    expect(row.textContent).toContain("provider process exited unexpectedly");
    // State is never colour alone -- the sign glyph carries it too, and this is the one row for
    // which nothing previously asserted it.
    expect(row.getAttribute("data-sign")).toBe("✗");
    /* This test started INPUT (`startedApp` presses `i`) and used to assert the textarea was
       DISABLED here. A disabled textarea is now not what a dead session leaves behind at all: the
       panel drops back to BROWSE, because INPUT on a dead session is an empty mode -- the box
       cannot take focus or a keystroke, and the key table drops everything there but `Escape`,
       including the `r` this very banner promises. The stronger property is that there is no box
       at all and the composer says which key does work. */
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("This session has ended.");
    // v1 polish item 7: said once, by the row.
    expect(container.textContent!.match(/press r/gi)).toHaveLength(1);
  });

  /* A session that ends normally is not an error, and must not read as one -- distinct row class,
     distinct (non-✗) sign, no error border. Round 1 review found the two had been unified onto
     `row-error`/`✗` on a misreading of spec §3.2; this pins them apart. */
  it("announces an ordinary session close without the lost-session treatment", () => {
    const { container } = startedApp();
    events({ type: "session_closed", reason: "provider exited" });
    const row = container.querySelector(".row-ended")!;
    expect(row).not.toBeNull();
    expect(row.textContent).toContain("provider exited");
    expect(row.getAttribute("data-sign")).toBe("·");
    expect(container.querySelector(".row-error")).toBeNull();
  });
});

/* R5 (v1 picks, Task 13): the band shows the ACTIVE tab's last reported usage -- every token and the
 *  cost, right of the model. Absent until a turn reports one and never `0 tok $0.00` for unknown; each
 *  report replaces the figure whole; and it is per tab, carried by the snapshot on a switch. The
 *  formatting itself is `band.test.ts`'s; this pins the wiring from events and snapshots to the band. */
describe("the band's usage segment (R5, v1 picks Task 13)", () => {
  const TWO = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
  const REPORT: UsageInfo = { total_cost_usd: 0.042, num_turns: null, tokens: { input: 1000, output: 3300, cache_creation: 0, cache_read: 0 }, model: null };
  const turnDone = (usage: UsageInfo | null, turn = "t1"): AgentDomainEvent => ({
    type: "turn_completed", turn_id: turn, outcome: "completed", result_text: "", stop_reason: null, usage,
  });
  const usageText = (container: HTMLElement) => container.querySelector(".band-usage")?.textContent ?? null;

  /** A live tab 1 with the band measured wide enough to draw every segment. */
  function started(state: AgentUiState = snapshotState()) {
    const widen = stubBandWidth();
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(state, 1);
    act(() => widen(rendered.container));
    return { ...rendered, widen };
  }
  function events(tab: number, ...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab, fromRevision: 0, throughRevision: list.length, events: list });
  }
  afterEach(() => vi.unstubAllGlobals());

  it("draws nothing before a report -- on an empty tab and on a live one that has reported nothing", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    act(() => widen(container));
    expect(container.querySelector(".band-usage")).toBeNull();

    dispatchLiveTab(snapshotState({ model: "claude-sonnet-5" }), 1);
    act(() => widen(container));
    // The band IS wide enough to draw the segment beside it: the model shows, the usage does not.
    expect(container.querySelector(".band-model")?.textContent).toBe("sonnet-5");
    expect(container.querySelector(".band-usage")).toBeNull();
    // A turn that reports nothing (interrupted, synthesized) is not a zero either.
    events(1, { type: "turn_started", turn_id: "t1" }, turnDone(null));
    expect(container.querySelector(".band-usage")).toBeNull();
  });

  it("a turn_completed's report shows all its tokens and its cost, right of the model", () => {
    const { container } = started(snapshotState({ model: "claude-sonnet-5" }));
    events(1, { type: "turn_started", turn_id: "t1" }, turnDone(REPORT));
    expect(usageText(container)).toBe("4.3k tok $0.04");
    const right = Array.from(container.querySelectorAll(".band-right .band-seg")).map((el) => el.className.replace("band-seg ", ""));
    expect(right.indexOf("band-usage")).toBe(right.indexOf("band-model") + 1);
    expect(container.querySelector<HTMLElement>(".band-usage")!.title).toContain("since this tab started or resumed");
    expect(container.querySelector<HTMLElement>(".band-usage")!.title).toContain("input 1,000 · output 3,300 · cache write 0 · cache read 0 · $0.0420");
  });

  it("a later turn that reports none keeps the figure, and a lower report replaces it (/clear)", () => {
    const { container } = started();
    events(1, turnDone(REPORT));
    events(1, turnDone(null, "t2"));
    expect(usageText(container)).toBe("4.3k tok $0.04");
    events(1, turnDone({ ...REPORT, total_cost_usd: 0.0012, tokens: { input: 40, output: 60, cache_creation: 0, cache_read: 0 } }, "t3"));
    expect(usageText(container)).toBe("100 tok <$0.01");
  });

  it("a legacy report -- a cost and a turn count, no tokens -- shows the cost alone", () => {
    const { container } = started();
    events(1, turnDone({ total_cost_usd: 1.5, num_turns: 4, tokens: null, model: null }));
    expect(usageText(container)).toBe("$1.50");
  });

  it("is per tab: a switch shows the other tab's snapshot figure, and back again", () => {
    const { container, widen } = started();
    events(1, turnDone(REPORT));
    expect(usageText(container)).toBe("4.3k tok $0.04");

    // Tab 2 has reported nothing: no figure, and above all not tab 1's left over.
    dispatch({ kind: "tabs", active: 2, tabs: TWO });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ usage: null }) });
    act(() => widen(container));
    expect(container.querySelector(".band-usage")).toBeNull();

    // Back on tab 1, whose snapshot carries the figure the panel itself no longer holds.
    dispatch({ kind: "tabs", active: 1, tabs: TWO });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ usage: REPORT }) });
    act(() => widen(container));
    expect(usageText(container)).toBe("4.3k tok $0.04");
  });

  it("a panel reload's first snapshot restores the figure at once", () => {
    const { container } = started(snapshotState({ usage: { ...REPORT, total_cost_usd: 12.5 } }));
    expect(usageText(container)).toBe("4.3k tok $12.50");
  });

  it("a report for a tab that is not on screen changes nothing here", () => {
    const { container } = started();
    dispatch({ kind: "tabs", active: 1, tabs: TWO });
    events(2, turnDone(REPORT));
    expect(container.querySelector(".band-usage")).toBeNull();
  });
});

/* Owner trial item 2 (2026-09-28, `the private review notes` §2): a bare
 *  /model or /effort now sends as a turn (never held back), and the panel opens a picker from its
 *  reply -- `../slashPicker`'s own unit tests cover the parsing itself; this only pins the App-level
 *  wiring (which reply arms it, what Enter/Esc do to a real send). */
describe("slash command pickers (owner trial item 2, 2026-09-28)", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    enterInputMode(rendered.container);
    return rendered;
  }

  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }

  // Verbatim probe reply, CLI 2.1.283 under a test-account wrapper.
  const MODEL_REPLY =
    "Current model: `Haiku 4.5` (effort: high)\n" +
    "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
    "fable[1m], opusplan, default, or a full model ID.";
  const EFFORT_REPLY = "Usage: /effort <low|medium|high|xhigh|max|auto>";

  function sendBareCommand(container: HTMLElement, command: string) {
    fireEvent.change(container.querySelector("textarea")!, { target: { value: command } });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
  }

  it("sends a bare /model as an ordinary turn -- it is no longer held back", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/model");
    expect(lastOfType("send_message")!.text).toBe("/model");
    expect(container.querySelector(".composer-slash-flash")).toBeNull();
  });

  it("opens a picker once the reply parses, current model marked, and Enter sends the cursor's choice", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/model");
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    );
    const picker = container.querySelector(".slash-picker")!;
    expect(picker).not.toBeNull();
    const current = picker.querySelector(".slash-picker-row.current")!;
    expect(current.textContent).toContain("haiku");
    expect(current.textContent).toContain("(current)");
    fireEvent.keyDown(picker, { key: "Enter" });
    expect(lastOfType("send_message")!.text).toBe("/model haiku");
    expect(container.querySelector(".slash-picker")).toBeNull();
  });

  it("Esc closes the picker and sends nothing", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/model");
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    );
    const sentBefore = posted.filter((m) => m.type === "send_message").length;
    fireEvent.keyDown(container.querySelector(".slash-picker")!, { key: "Escape" });
    expect(container.querySelector(".slash-picker")).toBeNull();
    expect(posted.filter((m) => m.type === "send_message")).toHaveLength(sentBefore);
  });

  it("opens a picker for a bare /effort too, with no current marked (the bare reply names none)", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/effort");
    expect(lastOfType("send_message")!.text).toBe("/effort");
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: EFFORT_REPLY, stop_reason: null, usage: null },
    );
    const picker = container.querySelector(".slash-picker")!;
    expect(picker).not.toBeNull();
    expect(picker.querySelectorAll(".slash-picker-current-marker")).toHaveLength(0);
    expect(picker.querySelector(".slash-picker-hint")!.textContent).toContain("applies to this session only");
    fireEvent.keyDown(picker, { key: "Enter" });
    expect(lastOfType("send_message")!.text).toBe("/effort low");
  });

  it("opens no picker when the reply does not parse -- shows as ordinary text instead", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/model");
    events(
      { type: "turn_started", turn_id: "t1" },
      {
        type: "turn_completed",
        turn_id: "t1",
        outcome: "completed",
        result_text: "I don't understand that command.",
        stop_reason: null,
        usage: null,
      },
    );
    expect(container.querySelector(".slash-picker")).toBeNull();
  });

  it("/model <name>, typed directly, still works exactly as before -- no picker, no special-casing", () => {
    const { container } = startedApp();
    sendBareCommand(container, "/model sonnet");
    expect(lastOfType("send_message")!.text).toBe("/model sonnet");
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    );
    // The reply to `/model sonnet` never arms a picker: only a BARE send does.
    expect(container.querySelector(".slash-picker")).toBeNull();
  });

  /** Whole-branch review finding 4 (v1 trial, 2026-09-28): a reply that landed while the chooser
   *  (or `?`, or the `/` prompt) was open opened the picker on top of it. `takeKeys` then gave the
   *  keys to the chooser underneath after a focus round trip, so Enter answered the chooser instead
   *  of choosing a model; and closing the picker left the keys on `<body>`. */
  describe("the picker and the other overlays (whole-branch review finding 4)", () => {
    const REPLY = (): AgentDomainEvent[] => [
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    ];
    const CHOOSER = {
      kind: "chooser",
      open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }],
      records: [],
    };
    const root = (container: HTMLElement) => container.querySelector<HTMLElement>(".agent-ui-conversation")!;

    it("a reply that lands under the chooser opens no picker, and the chooser keeps the keys", () => {
      const { container } = startedApp();
      sendBareCommand(container, "/model");
      dispatch(CHOOSER);
      events(...REPLY());
      expect(container.querySelector(".slash-picker")).toBeNull();
      expect(container.querySelector(".chooser")).not.toBeNull();
      dispatch({ kind: "pane_focus", focused: false });
      dispatch({ kind: "pane_focus", focused: true });
      expect(document.activeElement).toBe(container.querySelector(".chooser"));
    });

    it("opening the chooser drops the pending picker: its reply opens nothing after the chooser closes", () => {
      const { container } = startedApp();
      sendBareCommand(container, "/model");
      dispatch(CHOOSER);
      fireEvent.keyDown(container.querySelector(".chooser")!, { key: "Escape" });
      events(...REPLY());
      expect(container.querySelector(".slash-picker")).toBeNull();
    });

    for (const [overlay, key, selector] of [
      ["?", "?", ".keymap-overlay"],
      ["the / prompt", "/", ".search-bar"],
      ["the : line", ":", ".search-bar"],
    ] as const) {
      it(`a reply that lands under ${overlay} opens no picker over it`, () => {
        const { container } = startedApp();
        sendBareCommand(container, "/model");
        const textarea = container.querySelector("textarea");
        if (textarea !== null) fireEvent.keyDown(textarea, { key: "Escape" });
        fireEvent.keyDown(root(container), { key });
        expect(container.querySelector(selector)).not.toBeNull();
        events(...REPLY());
        expect(container.querySelector(".slash-picker")).toBeNull();
        expect(container.querySelector(selector)).not.toBeNull();
      });
    }

    for (const [how, key] of [
      ["Esc", "Escape"],
      ["Enter", "Enter"],
    ] as const) {
      it(`closing the picker with ${how} hands the keys back to the conversation`, () => {
        const { container } = startedApp();
        sendBareCommand(container, "/model");
        events(...REPLY());
        const picker = container.querySelector<HTMLElement>(".slash-picker")!;
        expect(document.activeElement).toBe(picker);
        fireEvent.keyDown(picker, { key });
        expect(container.querySelector(".slash-picker")).toBeNull();
        expect(document.activeElement).toBe(root(container));
      });
    }
  });

  /** K01 fix round 2 (review): the picker is a cancel route as the chooser is. Its keys return from
   *  `onKeyDown` ahead of the read-and-reset of a waiting prefix and count, and opening it dropped
   *  nothing, so a `g`/`z`/`[`/`]` typed while the reply was on its way took the first key after the
   *  picker: `i` opened no composer, and `f` could have been `gf`. A count and the leader outlived it
   *  the same way. Opening the picker now drops all three (`dropPendingKeys`). */
  describe("the picker drops a waiting prefix, count or leader sequence (K01 fix round 2)", () => {
    const REPLY = (): AgentDomainEvent[] => [
      { type: "turn_started", turn_id: "t1" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    ];
    const root = (container: HTMLElement) => container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    const current = (container: HTMLElement) => container.querySelector(".row-current .row-body")?.textContent ?? null;
    const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
    beforeEach(() => {
      vi.useFakeTimers();
    });
    afterEach(() => {
      vi.useRealTimers();
    });
    /** Sends a bare `/model`, then leaves the composer for BROWSE, where the keys under test go. */
    function awaitingReply(container: HTMLElement) {
      sendBareCommand(container, "/model");
      fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
      expect(container.querySelector("textarea")).toBeNull();
      expect(document.activeElement).toBe(root(container));
      wait(1000);
    }
    function pickerClosedByEsc(container: HTMLElement) {
      const picker = container.querySelector<HTMLElement>(".slash-picker");
      expect(picker).not.toBeNull();
      fireEvent.keyDown(picker!, { key: "Escape" });
      expect(container.querySelector(".slash-picker")).toBeNull();
      expect(document.activeElement).toBe(root(container));
      wait(1000);
    }

    for (const prefix of ["g", "z", "[", "]"]) {
      it(`${prefix}, then the reply's picker: its box closes, and after Esc, i opens the composer`, () => {
        const { container } = startedApp();
        awaitingReply(container);
        fireEvent.keyDown(root(container), { key: prefix });
        wait(1000);
        expect(container.querySelector(".which-key-box")).not.toBeNull();
        events(...REPLY());
        expect(container.querySelector(".which-key-box")).toBeNull();
        pickerClosedByEsc(container);
        fireEvent.keyDown(root(container), { key: "i" });
        expect(container.querySelector("textarea")).not.toBeNull();
      });
    }

    it("a count, then the reply's picker: after Esc, G goes to the last row, never row 2", () => {
      const { container } = startedApp();
      events(
        { type: "user_prompt_submitted", text: "r0" },
        { type: "user_prompt_submitted", text: "r1" },
        { type: "user_prompt_submitted", text: "r2" },
      );
      awaitingReply(container);
      fireEvent.keyDown(root(container), { key: "2" });
      events(...REPLY());
      pickerClosedByEsc(container);
      fireEvent.keyDown(root(container), { key: "G", shiftKey: true });
      const bodies = container.querySelectorAll('.message-list [data-nav-stop="row"] .row-body');
      expect(current(container)).not.toBe("r1");
      expect(current(container)).toBe(bodies[bodies.length - 1].textContent);
    });

    it("the leader, then the reply's picker: its box closes, and m after Esc runs no <leader>m", () => {
      const { container } = startedApp();
      dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
      awaitingReply(container);
      fireEvent.keyDown(root(container), { key: " " });
      wait(WHICH_KEY_DELAY_MS + 100);
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      events(...REPLY());
      expect(container.querySelector(".which-key-box")).toBeNull();
      pickerClosedByEsc(container);
      fireEvent.keyDown(root(container), { key: "m" });
      wait(1000);
      expect(posted.filter((m) => m.type === "cycle_mode")).toEqual([]);
    });
  });

  /** Fix round (Codex review findings): `pendingSlashPickerRef` is a single flag, not scoped to a
   *  tab, and `slashPicker` itself used to survive a tab switch -- both closed at the tab-switch
   *  reset block in `App.tsx`. */
  describe("tab switch (fix round)", () => {
    const TAB2 = { ...LIVE_TAB, id: 2, number: 2, label: "2 new" };

    it("closes an open picker on a tab switch, rather than leaving it targeting the wrong tab", () => {
      const { container } = startedApp();
      dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB, TAB2] });
      sendBareCommand(container, "/model");
      events(
        { type: "turn_started", turn_id: "t1" },
        { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
      );
      expect(container.querySelector(".slash-picker")).not.toBeNull();
      dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, TAB2] });
      expect(container.querySelector(".slash-picker")).toBeNull();
    });

    it("does not spuriously open a picker for a DIFFERENT tab's own reply after a switch mid-flight", () => {
      const { container } = startedApp();
      dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB, TAB2] });
      // Armed by the bare send, then switched away before tab 1's own `turn_completed` arrives --
      // that envelope is tab-scoped and gets dropped entirely (`acceptsEnvelope`), so without the
      // tab-switch reset the ref would sit armed for whatever `turn_completed` tab 2 sees next.
      sendBareCommand(container, "/model");
      dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, TAB2] });
      dispatch({
        kind: "events",
        tab: 2,
        fromRevision: 0,
        throughRevision: 2,
        events: [
          { type: "turn_started", turn_id: "t2" },
          // Shaped exactly like a `/model` reply, on purpose: the point of this test is that a
          // stale ref would open a picker here even though tab 2 never sent a bare `/model` at all.
          { type: "turn_completed", turn_id: "t2", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
        ],
      });
      expect(container.querySelector(".slash-picker")).toBeNull();
    });
  });
});

/* The in-flight motion indicator's elapsed clock (2026-09-20-in-flight-motion-design.md §8.4).
   `turnClock`'s PROVENANCE (a real `turn_started` event vs a turn id first seen inside a snapshot)
   is exactly the distinction `App.tsx` -- not the reducer, and not `ActivityLine`/`TurnActivity`
   (V2, session tabs Task 10; formerly `StatusLine`) -- is positioned to know, since only the raw
   envelope carries it. Tested here, at the layer that actually decides it. */
describe("the in-flight motion indicator's elapsed clock", () => {
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }

  it("is exact when the turn id was learned from a real turn_started event", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    events({ type: "turn_started", turn_id: "t1" });
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s");
  });

  it("is inexact when the turn id first arrives inside a snapshot -- a reload, or a resync mid-turn", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 1);
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s+");
  });

  it("does not restart -- keyed on the turn id, so a resync that repeats it leaves `since` untouched", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 1);
      act(() => {
        vi.advanceTimersByTime(5000);
      });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("5s+");
      // The same turn id again, as a resync mid-turn resends it: the clock keeps counting from the
      // original `since` rather than starting over from "now".
      dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 2);
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("5s+");
    } finally {
      vi.useRealTimers();
    }
  });

  it("is cleared by a terminal event, and a later turn starts its own clock from zero", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    events({ type: "turn_started", turn_id: "t1" });
    events({
      type: "turn_completed", turn_id: "t1", outcome: "completed",
      result_text: "", stop_reason: null, usage: null,
    });
    expect(container.querySelector(".turn-activity")).toBeNull();
    events({ type: "turn_started", turn_id: "t2" });
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s");
  });

  /* Whole-branch review, 2026-09-20: there are THREE whole-state resets in App.tsx, and only
     `returnToStartScreen` cleared the clock. The other two -- a terminal handoff and a fatal error
     -- left a stale `TurnClock` behind. Normally the next snapshot clears it incidentally via its
     `activeTurnId === null` branch, which is why this was invisible; the exposure is a snapshot
     arriving with an `activeTurnId` EQUAL to the stale one, which keeps both the old `since` and
     the old `exact: true` and shows an inflated elapsed time with no `+`. Whether a turn id can
     repeat across sessions is a question about ids this repo does not own (legacy mints uuid v4,
     the sidecar's arrive from Verdandi over the wire), so the resets clear it rather than rely on
     an answer. Both paths, driven end to end, with the repeated id as the probe. */
  for (const reset of ["a fatal error", "a terminal handoff"] as const) {
    it(`${reset} leaves no stale clock behind, even for a turn id that comes back`, () => {
      vi.useFakeTimers();
      try {
        const { container } = render(<App />);
        dispatch({ kind: "hello", ...HELLO });
        dispatchLiveTab(snapshotState(), 0);
        events({ type: "turn_started", turn_id: "t1" });
        act(() => {
          vi.advanceTimersByTime(60_000);
        });
        expect(container.querySelector(".turn-elapsed")?.textContent).toBe("60s");

        if (reset === "a fatal error") {
          dispatch({ kind: "error", tab: 1, message: "the session died" });
        } else {
          dispatch({
            kind: "handoff", tab: 1,
            command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
            cwd: "/home/user/project",
            providerSessionId: "1857dcd5-973b-46a2",
          });
        }
        expect(container.querySelector(".turn-activity")).toBeNull();

        // A new session whose first snapshot happens to carry the SAME turn id. Nothing was
        // observed starting it here, so the only honest reading is "at least 0 seconds" -- the
        // stale record would have said 60s, exactly, with no `+`.
        dispatch({ kind: "hello", ...HELLO });
        dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 9);
        expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s+");
      } finally {
        vi.useRealTimers();
      }
    });
  }

  /* The phase-3 GUI pass (2026-09-25): a switch away and back mid-turn read `0s+`, because the
     panel holds only the active tab and restarted the clock at the snapshot. The tab set now keeps
     when the turn started and the snapshot carries it (`turnStartedAtMs`). */
  it("keeps a running turn's real elapsed time across a tab switch", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState(), 0);
      const started = Date.now();
      events({ type: "turn_started", turn_id: "t1" });
      act(() => {
        vi.advanceTimersByTime(42_000);
      });
      const two = { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" } as const;
      dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, two] });
      expect(container.querySelector(".turn-activity")).toBeNull();
      dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB, two] });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ activeTurnId: "t1" }), turnStartedAtMs: started });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("42s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("takes a reload's start time from the snapshot when Rust sends one", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state: snapshotState({ activeTurnId: "t1" }), turnStartedAtMs: Date.now() - 7_500 });
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("7s");
  });

  // A snapshot with no `turnStartedAtMs` (an older build) keeps the old, honest reading.
  it("without a start time from Rust, a panel reload re-reads 0s+ and counts up from there -- less information, never a false statement", () => {
    // A "reload" here is simply a fresh App mount receiving its first snapshot with a turn already
    // active -- `turnClock` starts at `null` on every mount, same as `state` starts at
    // `initialState()`, and there is no persisted copy of it to restore.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "already-running" }), 5);
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s+");
  });
});

/* The keyboard skeleton: BROWSE/INPUT and the seven keys, wired end to end through `App.tsx`'s
   `onKeyDown` rather than exercised in isolation the way `keymap.test.ts` exercises `resolveKey`
   itself. This is the layer that proves the wiring, not just the table. */
describe("App keyboard: BROWSE/INPUT and the cursor", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    return rendered;
  }

  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }

  function conversationRoot(container: HTMLElement): HTMLElement {
    return container.querySelector(".agent-ui-conversation")!;
  }

  it("opens in BROWSE, where the composer offers a hint instead of a textarea", () => {
    const { container } = startedApp();
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-browse-hint")).not.toBeNull();
  });

  it("enters INPUT on i and leaves it on a non-composing Escape", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "i" });
    expect(container.querySelector("textarea")).not.toBeNull();

    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-browse-hint")).not.toBeNull();
  });

  /** rc.3 minors 6: a chord (Alt+g, Meta+z) is not the `g`/`z` prefix, so it must not swallow the plain key
   *  after it -- `i` here, which the armed prefix used to cancel. */
  it.each([["g", { altKey: true }], ["g", { metaKey: true }], ["z", { altKey: true }], ["z", { metaKey: true }]])(
    "enters INPUT on i right after %s with %j held: the chord armed no prefix",
    (first, held) => {
      const { container } = startedApp();
      fireEvent.keyDown(conversationRoot(container), { key: first, ...held });
      fireEvent.keyDown(conversationRoot(container), { key: "i" });
      expect(container.querySelector("textarea")).not.toBeNull();
    },
  );

  it("gives a composing Escape back to the input method instead of leaving INPUT", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape", isComposing: true });
    expect(container.querySelector("textarea")).not.toBeNull();
  });

  it("moves the cursor with j/k, and y copies the item under it", () => {
    const { container } = startedApp();
    const clipboard = stubClipboard();
    events({ type: "user_prompt_submitted", text: "first prompt" });
    events({ type: "user_prompt_submitted", text: "second prompt" });
    // R1 lands the cursor on "second prompt" the moment it arrives; `gg` returns to the top so the
    // `j`/`k` walk below still starts from a known row.
    fireEvent.keyDown(conversationRoot(container), { key: "g" });
    fireEvent.keyDown(conversationRoot(container), { key: "g" });

    fireEvent.keyDown(conversationRoot(container), { key: "j" });
    fireEvent.keyDown(conversationRoot(container), { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledWith("second prompt");

    fireEvent.keyDown(conversationRoot(container), { key: "k" });
    fireEvent.keyDown(conversationRoot(container), { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledWith("first prompt");
  });

  it("does not throw on y when the environment has no clipboard at all", () => {
    const { container } = startedApp();
    events({ type: "user_prompt_submitted", text: "only one" });
    expect(() => fireEvent.keyDown(conversationRoot(container), { key: "y" })).not.toThrow();
  });

  it("Enter toggles the folded tool result under the cursor", () => {
    const { container } = startedApp();
    events(
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "echo hi" } },
      { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "hi", is_error: false },
    );
    expect(container.querySelector('[data-folded="true"]')).not.toBeNull();

    fireEvent.keyDown(conversationRoot(container), { key: "Enter" });
    expect(container.querySelector('[data-folded="true"]')).toBeNull();

    fireEvent.keyDown(conversationRoot(container), { key: "Enter" });
    expect(container.querySelector('[data-folded="true"]')).not.toBeNull();
  });

  /* v1 picks, Task 4 (vim `:help za`/`zo`/`zc`): `za` flips the fold under the cursor exactly as Enter
     does, `zo` opens it and `zc` closes it, and neither moves a fold that is already the way it asks. */
  it("za toggles the folded tool result as Enter does; zo opens it and zc closes it, each idempotent", () => {
    const { container } = startedApp();
    events(
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "echo hi" } },
      { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "hi", is_error: false },
    );
    const z = (second: string) => {
      fireEvent.keyDown(conversationRoot(container), { key: "z" });
      fireEvent.keyDown(conversationRoot(container), { key: second });
    };
    const folded = () => container.querySelector('[data-folded="true"]') !== null;
    expect(folded()).toBe(true);

    z("a");
    expect(folded(), "za opens a folded result").toBe(false);
    z("a");
    expect(folded(), "za closes an open one").toBe(true);

    z("o");
    expect(folded(), "zo opens it").toBe(false);
    z("o");
    expect(folded(), "zo again leaves it open").toBe(false);

    z("c");
    expect(folded(), "zc closes it").toBe(true);
    z("c");
    expect(folded(), "zc again leaves it closed").toBe(true);

    fireEvent.keyDown(conversationRoot(container), { key: "Enter" });
    expect(folded(), "Enter and za are one toggle: Enter opened it").toBe(false);
    z("a");
    expect(folded(), "...and za closes what Enter opened").toBe(true);
  });

  /* zo / zc on a row with no fold (a prompt) change nothing and break nothing, and the keys stay
     where they were -- the row is still the cursor's. */
  it("zo and zc on a row that folds nothing leave the conversation as it was", () => {
    const { container } = startedApp();
    events({ type: "user_prompt_submitted", text: "just a prompt" });
    const before = container.querySelector(".message-list")!.innerHTML;
    for (const second of ["o", "c", "a"]) {
      fireEvent.keyDown(conversationRoot(container), { key: "z" });
      fireEvent.keyDown(conversationRoot(container), { key: second });
    }
    expect(container.querySelector(".row-current .row-body")!.textContent).toBe("just a prompt");
    expect(container.querySelector(".message-list")!.innerHTML).toBe(before);
  });

  /** v1 polish F18: a call a saved rule answered says which rule, from the events envelope's
   *  `ruleNotes` and from a snapshot's `allowedByRule` (a call without one: toolRegistry.test). */
  it("names the rule that allowed a call, from events and from a snapshot", () => {
    const { container } = startedApp();
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 0,
      throughRevision: 2,
      // One call: consecutive calls fold into a run summary, which draws no per-call lines.
      events: [
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "git log -3" } },
        { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "abc", is_error: false },
      ],
      ruleNotes: [{ toolUseId: "toolu_1", rule: "Bash(git log *)" }],
    });
    const notes = () => [...container.querySelectorAll(".tool-rule-note")].map((n) => n.textContent);
    expect(notes()).toEqual(["allowed by rule Bash(git log *)"]);
    expect(container.querySelector('[data-tool-name="Bash"] .tool-rule-note code')?.textContent).toBe("Bash(git log *)");
    dispatchLiveTab(
      {
        ...snapshotState(),
        toolCalls: [
          { seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "git log -3" }, result: null, allowedByRule: "Bash(git log *)" },
        ],
      },
      1,
    );
    expect(notes()).toEqual(["allowed by rule Bash(git log *)"]);
  });

  /** v1 trial item 7: an in-project edit the acceptEdits fast path allowed renders its own row --
   *  never folded into a count-only run with the reads around it -- says "allowed by auto", from
   *  the events envelope's `autoNotes` and from a snapshot's `allowedByAuto` (a call without one:
   *  toolRegistry.test). */
  it("shows an edit the fast path allowed as its own row, diffed, and named auto -- from events and from a snapshot", () => {
    const { container } = startedApp();
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 0,
      throughRevision: 5,
      // Read, Edit, Read: without item 7's fold-stopping rule these three would fold into one
      // count-only run row and the edit's own diff would never appear at all.
      events: [
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_read1", name: "Read", input: { file_path: "a.rs" } },
        { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_read1", content: "old", is_error: false },
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_edit", name: "Edit", input: { file_path: "a.rs", old_string: "old", new_string: "new" } },
        { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_edit", content: "ok", is_error: false },
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_read2", name: "Read", input: { file_path: "b.rs" } },
        { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_read2", content: "b", is_error: false },
      ],
      autoNotes: ["toolu_edit"],
    });
    expect(container.querySelectorAll(".tool-card-run")).toHaveLength(0);
    const notes = () => [...container.querySelectorAll(".tool-rule-note")].map((n) => n.textContent);
    expect(notes()).toEqual(["allowed by auto"]);
    expect(container.querySelector('[data-tool-name="Edit"] .permission-card-edit-path')).not.toBeNull();
    dispatchLiveTab(
      {
        ...snapshotState(),
        toolCalls: [
          { seq: 0, toolUseId: "toolu_1", name: "Edit", input: { file_path: "a.rs", old_string: "old", new_string: "new" }, result: null, allowedByAuto: true },
        ],
      },
      1,
    );
    expect(notes()).toEqual(["allowed by auto"]);
  });

  it("does nothing on r while the session is still running", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "r" });
    expect(container.querySelector(".agent-ui-conversation")).not.toBeNull();
    expect(container.querySelector(".empty-tab")).toBeNull();
  });

  it("r posts reset_tab once the session has ended, and the tab going NotStarted brings the empty tab back", () => {
    const { container } = startedApp();
    events({ type: "session_closed", reason: "provider exited" });

    fireEvent.keyDown(conversationRoot(container), { key: "r" });
    expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });

    // Rust resets the tab in place (ruling 12) and says so with a fresh `tabs` envelope -- there is
    // no local start-screen reset here any more, and no `ready` is asked for (ruling 17: Rust
    // re-sends `hello` on its own whenever the set of open sessions changes).
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    expect(container.querySelector(".empty-tab")).not.toBeNull();
  });

  /* §4.2/the round-1 review note: GTK owns Ctrl+Shift+R (panel reload) in the capture phase, and
     this handler must not fight it for a chord it does not claim. `event.key` for Shift+R is the
     shifted character "R", which is why this is safe by construction rather than by an explicit
     modifier check -- this test pins that property so it stays true if the table ever changes. */
  it("does not preventDefault an unclaimed chord like Ctrl+Shift+R", () => {
    const { container } = startedApp();
    const notCancelled = fireEvent.keyDown(conversationRoot(container), { key: "R", ctrlKey: true, shiftKey: true });
    expect(notCancelled).toBe(true);
  });

  it("highlights the row at the cursor, and moves the highlight with j/k", () => {
    const { container } = startedApp();
    events(
      { type: "user_prompt_submitted", text: "first" },
      { type: "user_prompt_submitted", text: "second" },
    );
    // R1 lands the cursor on "second" the moment it arrives; `gg` returns to the top.
    fireEvent.keyDown(conversationRoot(container), { key: "g" });
    fireEvent.keyDown(conversationRoot(container), { key: "g" });
    expect(container.querySelectorAll(".row-current")).toHaveLength(1);
    expect(container.querySelector(".row-current")!.textContent).toContain("first");

    fireEvent.keyDown(conversationRoot(container), { key: "j" });
    expect(container.querySelectorAll(".row-current")).toHaveLength(1);
    expect(container.querySelector(".row-current")!.textContent).toContain("second");
  });

  /* THE DEFECT: "j无法在长输出内部下滑" -- half of it is that nothing ever scrolled the cursor
     into view at all, so once `j` moved the highlight past the bottom of the viewport, the key
     read as doing nothing. jsdom implements no layout, so this is the only half of that this
     environment can see: that the right element's `scrollIntoView` was actually called. Whether
     the row really lands in the viewport is a GUI check nobody has run yet. */
  it("scrolls the cursor row into view when j/k move the cursor", () => {
    const { container } = startedApp();
    events(
      { type: "user_prompt_submitted", text: "first" },
      { type: "user_prompt_submitted", text: "second" },
    );
    // R1 lands the cursor on "second" the moment it arrives; `gg` returns to the top.
    fireEvent.keyDown(conversationRoot(container), { key: "g" });
    fireEvent.keyDown(conversationRoot(container), { key: "g" });
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();

    fireEvent.keyDown(conversationRoot(container), { key: "j" });

    expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
    expect(container.querySelector(".row-current")!.textContent).toContain("second");
  });

  /* The other half of "j无法在长输出内部下滑": a long tool result opened past its 260px fold
     (`.tool-result-body`) had no keyboard route into itself at all, only a mouse wheel. `j`/`k`
     must scroll THAT box first and only move the cursor off the row once the box has nothing
     further to give in the pressed direction -- see `scrollCursorRowBox` in `App.tsx`. */
  describe("j/k scroll a long tool result before moving off it", () => {
    function startedAppWithExpandedResult() {
      const rendered = startedApp();
      events(
        { type: "user_prompt_submitted", text: "before" },
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "long" } },
        { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "a lot of output", is_error: false },
        { type: "user_prompt_submitted", text: "after" },
      );
      const root = conversationRoot(rendered.container);
      // R1 lands the cursor on "after" the moment it arrives; `gg` returns to the top so the `j`
      // below still lands on the tool row, as it always did.
      fireEvent.keyDown(root, { key: "g" });
      fireEvent.keyDown(root, { key: "g" });
      fireEvent.keyDown(root, { key: "j" }); // cursor: prompt "before" -> the tool row
      fireEvent.keyDown(root, { key: "Enter" }); // unfold its result, so `.tool-result-body` exists
      expect(rendered.container.querySelector(".row-current .tool-result-body")).not.toBeNull();
      return rendered;
    }

    /* jsdom implements no layout: `scrollHeight`/`clientHeight` read 0 for every element, which is
       why `scrollCursorRowBox` always sees "no room to scroll" unless a test overrides them, as
       these two do -- not the true browser geometry, just enough to drive the same arithmetic. */
    function makeScrollable(
      box: HTMLElement,
      { scrollHeight, clientHeight, boxTop = 100 }: { scrollHeight: number; clientHeight: number; boxTop?: number },
    ) {
      Object.defineProperty(box, "scrollHeight", { value: scrollHeight, configurable: true });
      Object.defineProperty(box, "clientHeight", { value: clientHeight, configurable: true });
      Object.defineProperty(box, "scrollTop", { value: 0, configurable: true, writable: true });
      // Where the box sits relative to the list's viewport -- on screen unless a test says
      // otherwise. jsdom's all-zero rects would read as off screen.
      const rect = (top: number, bottom: number) => ({ top, bottom }) as DOMRect;
      box.getBoundingClientRect = () => rect(boxTop, boxTop + clientHeight);
      // The row around the box sits where the box does. Since `scrollCursorRow` (2026-09-19) also
      // measures the ROW, a row left at jsdom's all-zero rect would read as off screen here.
      const row = box.closest(".row-current") as HTMLElement;
      row.getBoundingClientRect = () => rect(boxTop, boxTop + clientHeight);
      const list = box.closest(".message-list") as HTMLElement;
      list.getBoundingClientRect = () => rect(0, 800);
    }

    it("scrolls the box instead of the cursor while it still has room to scroll", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260 });

      fireEvent.keyDown(conversationRoot(container), { key: "j" });

      // Still the tool row -- the cursor did not advance to "after".
      expect(container.querySelector(".row-current")!.textContent).toContain("long");
      expect(box.scrollTop).toBeGreaterThan(0);
    });

    it("moves to the next row once the box has reached its end", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 260, clientHeight: 260 }); // nothing left to scroll

      fireEvent.keyDown(conversationRoot(container), { key: "j" });

      expect(container.querySelector(".row-current")!.textContent).toContain("after");
    });

    /* Review finding: with the box scrolled off screen (the user mouse-scrolled the list away), `j`
       used to scroll the hidden box and change nothing visible. Now the first press brings the row
       back into view instead. */
    it("brings the row back into view, rather than scrolling a box that is off screen", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260, boxTop: -2000 });
      const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
      scrollIntoView.mockClear();

      fireEvent.keyDown(conversationRoot(container), { key: "j" });

      expect(box.scrollTop).toBe(0);
      expect(container.querySelector(".row-current")!.textContent).toContain("long");
      expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
    });

    it("scrolls the box upward on k, the same way", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260 });
      Object.defineProperty(box, "scrollTop", { value: 100, configurable: true, writable: true });

      fireEvent.keyDown(conversationRoot(container), { key: "k" });

      expect(container.querySelector(".row-current")!.textContent).toContain("long");
      expect(box.scrollTop).toBeLessThan(100);
    });

    /* v1 trial item 5: "Inside a long tool result's capped box, scroll that box first, the way j/k
       do" -- the same `scrollCursorRowBox` j/k already call, reused as-is. Unlike `j` (which moves
       the cursor to the NEXT row once the box is exhausted), Ctrl+e never advances the cursor row by
       itself: once the box has nothing left, the rest of the count scrolls the conversation under an
       unmoved cursor, and only leaving the view (checked elsewhere) would re-home it. */
    it("Ctrl+e scrolls the box first, the same way j does", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260 });

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      // Still the tool row -- the cursor did not advance to "after".
      expect(container.querySelector(".row-current")!.textContent).toContain("long");
      expect(box.scrollTop).toBeGreaterThan(0);
    });

    it("Ctrl+e falls through to the conversation once the box has reached its end, without moving the cursor", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 260, clientHeight: 260 }); // nothing left to scroll
      const list = box.closest(".message-list") as HTMLElement;

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      expect(box.scrollTop).toBe(0); // the box made no progress
      expect(list.scrollTop).toBeGreaterThan(0); // the unit fell through to the conversation instead
      // Unlike `j`, which would have moved on to "after" here (the test just above this one).
      expect(container.querySelector(".row-current")!.textContent).toContain("long");
    });

    it("Ctrl+y scrolls the box upward first, the same way k does", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260 });
      Object.defineProperty(box, "scrollTop", { value: 100, configurable: true, writable: true });

      fireEvent.keyDown(conversationRoot(container), { key: "y", ctrlKey: true });

      expect(container.querySelector(".row-current")!.textContent).toContain("long");
      expect(box.scrollTop).toBeLessThan(100);
    });

    /* Codex review, v1 trial item 5 fix round, finding 2: a multi-line command taller than the view,
       scrolled into so the ROW spans both viewport edges, with its `.tool-result-body` further down
       still off screen. `scrollCursorRowBox` saw the box off screen and called
       `row.scrollIntoView({block:"nearest"})`, but "nearest" moves nothing once the row already
       spans the viewport it would be aligned against -- so the press was still claimed (returning
       `true`), and `Ctrl+e` never fell through to scroll the conversation itself. */
    it("Ctrl+e falls through to the conversation when the row already spans the view and its box is still off screen", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      const row = container.querySelector(".row-current") as HTMLElement;
      const list = box.closest(".message-list") as HTMLElement;
      Object.defineProperty(box, "scrollHeight", { value: 500, configurable: true });
      Object.defineProperty(box, "clientHeight", { value: 260, configurable: true });
      Object.defineProperty(box, "scrollTop", { value: 0, configurable: true, writable: true });
      // The row spans both edges of the 800px viewport; its result box sits further down, entirely
      // below the bottom edge -- exactly the geometry the review's own repro describes.
      row.getBoundingClientRect = () => ({ top: -50, bottom: 850 }) as DOMRect;
      box.getBoundingClientRect = () => ({ top: 820, bottom: 1080 }) as DOMRect;
      list.getBoundingClientRect = () => ({ top: 0, bottom: 800 }) as DOMRect;
      Object.defineProperty(list, "scrollTop", { value: 0, configurable: true, writable: true });

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      expect(box.scrollTop).toBe(0); // the box never moved -- `scrollIntoView` could not reach it
      expect(list.scrollTop).toBeGreaterThan(0); // the press fell through to the conversation instead
    });

    /* Codex review, v1 trial item 5 fix round, finding 3: at the true bottom already, `Ctrl+y` then
       `Ctrl+e` -- both entirely absorbed by the row's own `.tool-result-body`, back to where it
       started -- never touch the OUTER list at all. `noteUserScroll("down")` alone only arms
       `MessageList`'s steering window; only a real `scroll` event lets its own "did this reach the
       bottom" rule re-arm following, and a box-only scroll never fires one on `.message-list` (a
       `scroll` event does not bubble from a nested scrollable). Before the fix, following stayed off
       for good even though the box round-tripped back to exactly where it began. */
    it("Ctrl+e dispatches a scroll event on the list even when the whole press stayed inside the box", () => {
      const { container } = startedAppWithExpandedResult();
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      makeScrollable(box, { scrollHeight: 500, clientHeight: 260 });
      Object.defineProperty(box, "scrollTop", { value: 240, configurable: true, writable: true }); // already at its own end
      const list = box.closest(".message-list") as HTMLElement;
      const scrollEvents: Event[] = [];
      list.addEventListener("scroll", (e) => scrollEvents.push(e));

      fireEvent.keyDown(conversationRoot(container), { key: "y", ctrlKey: true }); // up: stops following outright
      expect(box.scrollTop).toBe(200); // absorbed entirely by the box
      expect(list.scrollTop).toBe(0);

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true }); // back down to its end

      expect(box.scrollTop).toBe(240); // the box round-tripped back to where it started
      expect(list.scrollTop).toBe(0); // the outer list was never touched
      // Without the fix, no `scroll` event ever reaches the list here, so `MessageList` never learns
      // it is still at the true bottom and following stays off for good.
      expect(scrollEvents.length).toBeGreaterThan(0);
    });

    /* Whole-branch review finding 3 (v1 trial, 2026-09-28): Ctrl+e/Ctrl+y reused `scrollCursorRowBox`,
       j/k's own helper, whose off-screen branch brings the cursor row back into view and claims the
       unit. vim's CTRL-E/CTRL-Y never move the view toward the cursor, so held Ctrl+y with the row's
       box just below the view jumped back down to the row every time it scrolled it off (it never got
       past the row), and Ctrl+e jumped a whole box height. For a line scroll the box now takes a unit
       only while at least part of it is on screen and it can still move that way. The reviewers'
       geometry: list 0-800, row 760-1080, box 800-1060, the list scrolled to 400. */
    function placeBoxBelowTheView(container: HTMLElement) {
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      const row = container.querySelector(".row-current") as HTMLElement;
      const list = box.closest(".message-list") as HTMLElement;
      Object.defineProperty(box, "scrollHeight", { value: 500, configurable: true });
      Object.defineProperty(box, "clientHeight", { value: 260, configurable: true });
      Object.defineProperty(box, "scrollTop", { value: 100, configurable: true, writable: true });
      row.getBoundingClientRect = () => ({ top: 760, bottom: 1080 }) as DOMRect;
      box.getBoundingClientRect = () => ({ top: 800, bottom: 1060 }) as DOMRect;
      list.getBoundingClientRect = () => ({ top: 0, bottom: 800 }) as DOMRect;
      Object.defineProperty(list, "scrollTop", { value: 400, configurable: true, writable: true });
      return { box, list };
    }

    it("held Ctrl+y scrolls the conversation up a line each time, never back down to a box below the view", () => {
      const { container } = startedAppWithExpandedResult();
      const { box, list } = placeBoxBelowTheView(container);
      const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
      scrollIntoView.mockClear();

      const seen: number[] = [];
      for (let i = 0; i < 3; i++) {
        fireEvent.keyDown(conversationRoot(container), { key: "y", ctrlKey: true, repeat: i > 0 });
        seen.push(list.scrollTop);
      }

      expect(scrollIntoView).not.toHaveBeenCalled();
      expect(box.scrollTop).toBe(100); // the box is off screen: it takes nothing
      expect(seen[0]).toBeLessThan(400);
      expect(seen[1]).toBeLessThan(seen[0]);
      expect(seen[2]).toBeLessThan(seen[1]);
    });

    it("Ctrl+e with the box below the view scrolls the conversation one line, not to the box", () => {
      const { container } = startedAppWithExpandedResult();
      const { box, list } = placeBoxBelowTheView(container);
      const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
      scrollIntoView.mockClear();

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      expect(scrollIntoView).not.toHaveBeenCalled();
      expect(box.scrollTop).toBe(100);
      expect(list.scrollTop).toBeGreaterThan(400);
      expect(list.scrollTop).toBeLessThan(400 + 40); // one line, not a box height
    });

    it("Ctrl+e scrolls a box that is partly on screen", () => {
      const { container } = startedAppWithExpandedResult();
      const { box, list } = placeBoxBelowTheView(container);
      box.getBoundingClientRect = () => ({ top: 700, bottom: 960 }) as DOMRect;

      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      expect(box.scrollTop).toBeGreaterThan(100);
      expect(list.scrollTop).toBe(400);
    });

    /* Codex, the same finding: with the row's top exactly on the list's top (a tall row, scrolled to),
       the "row spans the view" check was strict, missed the equality, and every unit of a counted
       Ctrl+e went to a `scrollIntoView` that moved nothing. `j` had the same equality hole. */
    function alignRowTopWithTheList(container: HTMLElement) {
      const box = container.querySelector(".row-current .tool-result-body") as HTMLElement;
      const row = container.querySelector(".row-current") as HTMLElement;
      const list = box.closest(".message-list") as HTMLElement;
      Object.defineProperty(box, "scrollHeight", { value: 500, configurable: true });
      Object.defineProperty(box, "clientHeight", { value: 260, configurable: true });
      Object.defineProperty(box, "scrollTop", { value: 0, configurable: true, writable: true });
      row.getBoundingClientRect = () => ({ top: 0, bottom: 1100 }) as DOMRect;
      box.getBoundingClientRect = () => ({ top: 820, bottom: 1080 }) as DOMRect;
      list.getBoundingClientRect = () => ({ top: 0, bottom: 800 }) as DOMRect;
      Object.defineProperty(list, "scrollTop", { value: 0, configurable: true, writable: true });
      return { box, list };
    }

    it("a counted Ctrl+e moves the conversation when the row's top sits exactly on the list's top", () => {
      const { container } = startedAppWithExpandedResult();
      const { box, list } = alignRowTopWithTheList(container);

      fireEvent.keyDown(conversationRoot(container), { key: "3" });
      fireEvent.keyDown(conversationRoot(container), { key: "e", ctrlKey: true });

      expect(box.scrollTop).toBe(0);
      expect(list.scrollTop).toBeGreaterThan(0);
    });

    it("j steps through the row when its top sits exactly on the list's top and its box is below the view", () => {
      const { container } = startedAppWithExpandedResult();
      const { box, list } = alignRowTopWithTheList(container);

      fireEvent.keyDown(conversationRoot(container), { key: "j" });

      expect(box.scrollTop).toBe(0);
      expect(list.scrollTop).toBeGreaterThan(0);
      expect(container.querySelector(".row-current")!.textContent).toContain("long");
    });
  });

  it("writes nothing to the clipboard when the timeline is empty, rather than clobbering it with an empty string", () => {
    const { container } = startedApp();
    // No prompts/messages/tools/permissions at all -- timeline.length === 0, cursor stays 0.
    const clipboard = stubClipboard();
    fireEvent.keyDown(conversationRoot(container), { key: "y" });
    expect(clipboard.writeText).not.toHaveBeenCalled();
  });

  it("clamps the cursor when the timeline shrinks, e.g. a permission resolving", () => {
    const { container } = startedApp();
    const clipboard = stubClipboard();
    events(
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { a: 1 } },
      { type: "permission_requested", permission_id: "perm-2", tool_use_id: "toolu_2", tool_name: "Bash", input: { a: 2 } },
    );
    fireEvent.keyDown(conversationRoot(container), { key: "j" }); // cursor -> 1, on perm-2
    events({ type: "permission_resolved", permission_id: "perm-2", outcome: "allowed" }); // timeline shrinks to 1

    fireEvent.keyDown(conversationRoot(container), { key: "y" });
    // A cursor left at the old index 1 would find nothing (or, before this fix, silently write "").
    // Clamped to the new last row, it copies the one item that is still there.
    expect(clipboard.writeText).toHaveBeenCalledWith(JSON.stringify({ a: 1 }, null, 2));
  });

  /* Review round 1, item 1: this panel does not own every keystroke inside its own subtree -- only
     the ones outside a text field someone is actually typing into. A permission card's deny-reason
     box is the one that exists today. Both halves of the bug the reviewer reproduced in jsdom. */
  describe("does not fight a permission card's reason box for keystrokes", () => {
    function withAPermissionRequest() {
      const rendered = startedApp();
      events({ type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} });
      return rendered;
    }

    /* The BROWSE half: `j` used to be claimed (and `preventDefault`ed) as a cursor move before it
       ever reached the input, and `i` stole focus into the composer mid-word. */
    it("gives a full typed word to the box while in BROWSE", () => {
      const { container } = withAPermissionRequest();
      const reasonBox = container.querySelector(".permission-card input") as HTMLInputElement;
      typeIntoInput(reasonBox, "just not this");
      expect(reasonBox.value).toBe("just not this");
      // And the panel itself did not treat any of those letters as a command: still in BROWSE.
      expect(container.querySelector("textarea")).toBeNull();
    });

    /* The INPUT half: clicking the box blurred the composer's textarea, `onModeChange` reported
       BROWSE, and the (then-ungated) refocus effect yanked focus straight back out of the box the
       click had just placed it in -- the click landed nowhere. */
    it("keeps focus in the box when it is clicked while INPUT is active", () => {
      const { container } = withAPermissionRequest();
      fireEvent.keyDown(conversationRoot(container), { key: "i" });
      const textarea = container.querySelector("textarea")!;
      expect(document.activeElement).toBe(textarea);

      const reasonBox = container.querySelector(".permission-card input") as HTMLInputElement;
      // What a real click does: the browser blurs whatever had focus and focuses the clicked
      // element, in that order. jsdom's real `.focus()` reproduces both; a synthetic
      // `fireEvent.focus` (used elsewhere in this suite for callback-only assertions) does not
      // move `document.activeElement` at all, so it could not have caught this. `act` flushes the
      // resulting `onModeChange("browse")` and the effect it triggers before the assertion below.
      act(() => reasonBox.focus());

      expect(document.activeElement).toBe(reasonBox);
    });
  });

  /* Review round 1, item 3: the gap this task's own report flagged and the reviewer wrote the test
     for. `fireEvent` bypasses real focus entirely, which is exactly why a test that always
     dispatches at a hand-picked `conversationRoot(container)` cannot catch a regression here -- it
     would keep passing even if the refocus effect stopped firing and real focus were left on
     `document.body`. Asserting on, and dispatching at, `document.activeElement` itself is what
     makes this test depend on the effect actually running. */
  it("returns real DOM focus to the panel root after Escape, and j/y keep working from there", () => {
    const { container } = startedApp();
    events(
      { type: "user_prompt_submitted", text: "first" },
      { type: "user_prompt_submitted", text: "second" },
    );
    const root = conversationRoot(container);

    fireEvent.keyDown(root, { key: "i" });
    expect(container.querySelector("textarea")).not.toBeNull();
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });

    expect(document.activeElement).toBe(root);

    const clipboard = stubClipboard();
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    fireEvent.keyDown(document.activeElement!, { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledWith("second");
  });

  /* THE REGRESSION THE OWNER HIT ON AN INSTALLED BUILD, 2026-09-18: "approve 现在没有键位能够触及
     好像" -- no key reached Approve at all. `onKeyDown` claimed Enter from any non-editable target
     and called `preventDefault()`, and Enter's default action on a focused `<button>` IS its
     activation click, so the only keyboard route to a permission decision was deleted. (The spec's
     `a`/`d` allow/deny keys are a LATER sub-project and deliberately absent from this skeleton, so
     Tab-then-Enter was the whole route. Before this branch nothing listened for Enter and it
     worked.)

     READ THE ASSERTIONS LITERALLY, because they are not the bug: **jsdom does not implement button
     activation from a keydown at all.** `fireEvent.keyDown(button, {key: "Enter"})` fires no click
     in jsdom whether or not the panel swallows the key, so no test in this environment can assert
     "Approve was clicked" -- and a test that tried would have passed throughout the regression.
     What these pin is the GUARD, not the effect: the handler must not claim the key (its default
     survives, so a real browser's own activation runs) and must not do its own BROWSE action with
     it either. The effect itself is a GUI check; `shell/MANUAL_VERIFICATION.md` owes it.

     **K02 (v1 picks, ruling R3) changed this for a card's own three buttons**: Enter there is now
     claimed too (`:ls⏎` approved a card through the native activation), and the panel presses the
     button itself, `TYPING_GUARD_MS` after an Enter that follows a landing on it (`h`/`l`/Tab onto
     it, or a HINT). The regression stays closed, and is now pinned end to end -- jsdom does see a
     press the panel makes itself -- by `v1: typing never answers a card`'s K02 tests. What is left
     here: Enter with nothing landed on the button presses nothing, and every other control (Stop,
     a `<summary>`) keeps the browser's own activation. */
  describe("leaves a focused control's own keys alone (the Approve-unreachable regression)", () => {
    function withAToolCallAndAPermission() {
      const rendered = startedApp();
      events(
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
        { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
      );
      return rendered;
    }

    /* Every control a keyboard user can Tab to inside the conversation, by the label they read.
       Approve and Deny are the two that were reported; the rest share the one handler and would
       have failed the same way. */
    /* v1 S5 (spec 2026-09-27 §2.3) narrows this on purpose: Enter is still left to a card's own
       buttons -- the regression this block pins -- but Space on them is now claimed and does
       nothing. Enter is pressed with no key just before it, since S1 refuses one right after typing
       (spec §2.1); that half is `v1: typing never answers a card`'s. */
    it("claims Enter and Space on the card's own buttons, and an Enter nothing landed presses nothing (K02, v1 S5)", () => {
      vi.useFakeTimers();
      const widen = stubBandWidth();
      try {
        const { container } = withAToolCallAndAPermission();
        act(() => widen(container));
        for (const label of ["Approve", "Deny"]) {
          const button = buttonLabelled(container, label)!;
          expect(button, `no button labelled ${label}`).toBeDefined();
          act(() => vi.advanceTimersByTime(300));
          // fireEvent returns false exactly when the default was prevented.
          expect(fireEvent.keyDown(button, { key: "Enter" }), `${label} left Enter to the button`).toBe(false);
          expect(container.querySelector(".band-message")?.textContent).toBe(
            "Enter answers a card right after l, h or Tab onto its button",
          );
          act(() => vi.advanceTimersByTime(300));
          expect(fireEvent.keyDown(button, { key: " " }), `${label} left Space to the button`).toBe(false);
        }
        act(() => vi.advanceTimersByTime(1000));
        expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });

    /* The other half, and the one jsdom CAN observe end to end: a key that reaches this handler
       from inside a button must not run BROWSE's own action for it either. `y` (copy the item at
       the cursor) is used rather than Enter only because its effect is visible in this
       environment -- Enter's BROWSE action is "expand the current row", and a tool call with no
       result yet renders identically expanded or not, so it would assert nothing. The guard is one
       selector test shared by every key, so what holds for `y` holds for Enter. */
    /* Narrowed 2026-09-19, deliberately. This used to assert that NO key from a focused button runs a
       BROWSE action, with `y` standing in for Enter. Since every control became keyboard-reachable
       (`./nav`), a focused button is an ordinary place for the keys to be, so `h`/`j`/`k`/`l` from
       it must still navigate. What the original regression needs is narrower and is asserted
       directly: Enter and Space from a button are left to the button. */
    it("claims Enter (K02) and Space (v1 S5) on a focused Approve, and still navigates from it", () => {
      const { container } = withAToolCallAndAPermission();
      const approve = buttonLabelled(container, "Approve")!;
      // `fireEvent` returns false when the handler called preventDefault, i.e. claimed the key.
      expect(fireEvent.keyDown(approve, { key: "Enter" })).toBe(false);
      expect(fireEvent.keyDown(approve, { key: " " })).toBe(false);
      expect(fireEvent.keyDown(approve, { key: "k" })).toBe(false);
      expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
    });

    /* `<summary>` is the third activatable shape in this panel (the generic tool card's
       disclosure, `toolRegistry.tsx`) and Enter opens it by default too, so the guard is a
       selector over activatable controls rather than a special case for `<button>`. */
    it("leaves a <summary> disclosure's own Enter alone", () => {
      const { container } = startedApp();
      events({ type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_2", name: "Nonesuch", input: {} });
      const summary = container.querySelector("summary")!;
      expect(summary).not.toBeNull();
      expect(fireEvent.keyDown(summary, { key: "Enter" })).toBe(true);
    });
  });

  /* F2: INPUT was a one-way trap on an ended session while the banner kept promising `r`. */
  describe("a dead session leaves no mode that drops the key its own hints promise", () => {
    it("refuses i, and says so instead of promising it", () => {
      const { container } = startedApp();
      events({ type: "session_closed", reason: "provider exited" });
      fireEvent.keyDown(conversationRoot(container), { key: "i" });
      expect(container.querySelector("textarea")).toBeNull();
      const hint = container.querySelector(".composer-browse-hint")!;
      expect(hint.textContent).not.toContain("按 i");
      expect(hint.textContent).toBe("This session has ended.");
      expect(container.textContent!.match(/press r/gi)).toHaveLength(1);
      // Nothing for a Tab to land on either: the hint is a focus route into INPUT while the
      // session lives, and that route is what the `tabIndex` provides.
      expect(hint.hasAttribute("tabindex")).toBe(false);
    });

    it("drops back to BROWSE when the session dies while INPUT is active, and r still works", () => {
      const { container } = startedApp();
      fireEvent.keyDown(conversationRoot(container), { key: "i" });
      expect(container.querySelector("textarea")).not.toBeNull();

      events({ type: "session_unavailable", reason: "provider process exited unexpectedly" });
      expect(container.querySelector("textarea")).toBeNull();

      // The key the banner promises actually resolves, from wherever focus ended up.
      fireEvent.keyDown(document.activeElement!, { key: "r" });
      expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
      expect(container.querySelector(".empty-tab")).not.toBeNull();
    });
  });
});

/** The per-row which-key STRIP (`.which-key`) is gone (spec §5.1, ruling R4): the band leaves no
 *  room for it, and Claude Code itself only ever kept a bare `? for shortcuts`, which the owner's
 *  own ruling left out too (spec §5.4, "Discoverability"). The which-key BOX (`.which-key-box`,
 *  the leader/g-prefix popup) is a different feature and stays -- see the nested describe below. */
describe("App: the which-key box (panel round 2 plan, Task 10)", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });

  it("the per-row which-key strip no longer renders in BROWSE", () => {
    const { container } = started();
    act(() => root(container).focus());
    expect(container.querySelector(".which-key")).toBeNull();
  });

  describe("the g prefix box (panel round 2 plan, Task 8; spec §2.4, 200ms)", () => {
    // Correction, Task 8: the `g`/`z`/`[`/`]` prefixes no longer draw their own line in the
    // which-key STRIP -- every pending prefix now shows in the which-key BOX instead, after
    // `WHICH_KEY_DELAY_MS` (200ms, widened from the strip's old `g`-only 400ms).
    it("shows nothing before the delay, `first row` once it elapses, and clears on the next key", () => {
      vi.useFakeTimers();
      try {
        const { container } = started();
        events({ type: "user_prompt_submitted", text: "hi" });
        act(() => root(container).focus());
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
        expect(container.querySelector(".which-key-box")).toBeNull();
        act(() => vi.advanceTimersByTime(1));
        expect(container.querySelector(".which-key-box")!.textContent).toContain("first row");
        press("j");
        expect(container.querySelector(".which-key-box")).toBeNull();
      } finally {
        vi.useRealTimers();
      }
    });

    it("gg inside the delay never shows the box", () => {
      vi.useFakeTimers();
      try {
        const { container } = started();
        events({ type: "user_prompt_submitted", text: "hi" });
        act(() => root(container).focus());
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
        expect(container.querySelector(".which-key-box")).toBeNull();
      } finally {
        vi.useRealTimers();
      }
    });
  });
});

/* v1 picks, Task 6 (ruling R11): vim's `CTRL-W h/j/k/l` -- the keys to the module that way, from the live
   conversation's BROWSE. `Ctrl+w` is a reserved prefix (K01, R1): it waits for its next key with no
   timeout, and that key completes one of the four pairs -- posted to shell as `pane_nav`, which runs
   the move `Ctrl+h/j/k/l` do -- or is swallowed. Nothing here reaches a card: the answer side of it (a
   pause, then `a`/`d`; a landing, then Enter) is in "v1: typing never answers a card" below. */
describe("Ctrl+w h/j/k/l in BROWSE: the keys to the module that way (v1 picks, Task 6, R11)", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  function prompts(...texts: string[]) {
    events(...texts.map((text): AgentDomainEvent => ({ type: "user_prompt_submitted", text })));
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string, init: Record<string, unknown> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
  const ctrlW = () => press("w", { ctrlKey: true });
  const current = (c: HTMLElement) => c.querySelector(".row-current .row-body")!.textContent;
  const paneNavs = () => posted.filter((m) => m.type === "pane_nav");
  /** BROWSE on a conversation with a few rows, the keys on it. */
  function browsing() {
    const rendered = started();
    prompts("r0", "r1", "r2", "r3");
    act(() => root(rendered.container).focus());
    return rendered;
  }

  it.each([
    ["h", "left"],
    ["j", "down"],
    ["k", "up"],
    ["l", "right"],
  ])("Ctrl+w then %s posts one pane_nav %s, and the prefix alone posts nothing", (second, direction) => {
    browsing();
    ctrlW();
    expect(paneNavs()).toEqual([]);
    press(second);
    expect(paneNavs()).toEqual([{ type: "pane_nav", request_id: expect.any(String), direction }]);
    // The prefix is spent: a plain second key is a plain key again, not another move.
    press(second);
    expect(paneNavs()).toHaveLength(1);
  });

  it("reads a real keyboard's key events: the bare Control keydown, Ctrl+w, Control released, then l", () => {
    browsing();
    press("Control", { ctrlKey: true });
    ctrlW();
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "w" });
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "Control" });
    press("l");
    expect(lastOfType("pane_nav")).toMatchObject({ direction: "right" });
    expect(paneNavs()).toHaveLength(1);
  });

  it("gives every move its own request id", () => {
    browsing();
    ctrlW();
    press("l");
    ctrlW();
    press("h");
    const ids = paneNavs().map((m) => m.request_id);
    expect(ids).toHaveLength(2);
    expect(new Set(ids).size).toBe(2);
    expect(paneNavs().map((m) => m.direction)).toEqual(["right", "left"]);
  });

  it("a count typed before it is spent by it: 3 Ctrl+w l moves once, and the next j is one row", () => {
    const { container } = browsing();
    press("g");
    press("g");
    expect(current(container)).toBe("r0");
    press("3");
    ctrlW();
    press("l");
    expect(paneNavs()).toHaveLength(1);
    press("j");
    expect(current(container)).toBe("r1");
  });

  it("a key that completes no pair is swallowed with the prefix: Ctrl+w d, Ctrl+w Escape, Ctrl+w a post nothing", () => {
    const { container } = browsing();
    for (const k of ["d", "Escape", "a", "i", "y"]) {
      ctrlW();
      press(k);
    }
    expect(paneNavs()).toEqual([]);
    // Nothing ran as itself: `i` would have entered INPUT, `y` would have copied.
    expect(container.querySelector("textarea")).toBeNull();
    expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
  });

  it("does not arm on another Ctrl+w chord, so the l after it is a plain l", () => {
    browsing();
    for (const init of [{ ctrlKey: true, altKey: true }, { ctrlKey: true, metaKey: true }, { ctrlKey: true, shiftKey: true }]) {
      press("w", init);
      press("l");
    }
    expect(paneNavs()).toEqual([]);
  });

  it("a pane switch the page never saw as a key drops a waiting Ctrl+w, as it drops a g", () => {
    browsing();
    ctrlW();
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    press("l");
    expect(paneNavs()).toEqual([]);
  });

  it("shell claiming a Ctrl+j after the prefix (nav_key) drops it too: nothing is posted as a pane move", () => {
    const { container } = browsing();
    ctrlW();
    dispatch({ kind: "nav_key", direction: "down" });
    // `Ctrl+j` ran as itself -- INPUT -- and the prefix went with it.
    expect(container.querySelector("textarea")).not.toBeNull();
    press("Escape");
    press("l");
    expect(paneNavs()).toEqual([]);
  });

  it("is BROWSE's: in INPUT the composer keeps Ctrl+w, and no prefix is left waiting behind it", () => {
    vi.useFakeTimers();
    try {
      const { container } = browsing();
      press("i");
      expect(container.querySelector("textarea")).not.toBeNull();
      ctrlW();
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS + 100));
      expect(container.querySelector(".which-key-box")).toBeNull();
      press("l");
      expect(paneNavs()).toEqual([]);
      // Back in BROWSE the chord works again, and nothing is left over from INPUT.
      press("Escape");
      ctrlW();
      press("l");
      expect(paneNavs()).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("still moves from a session that ended: its transcript is a BROWSE panel with modules around it", () => {
    browsing();
    events({ type: "session_closed", reason: "provider exited" });
    ctrlW();
    press("h");
    expect(lastOfType("pane_nav")).toMatchObject({ direction: "left" });
  });

  it("is swallowed under the ? overlay, which owns every key, and works again once it closes", () => {
    const { container } = browsing();
    press("?", { shiftKey: true });
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
    ctrlW();
    press("l");
    expect(paneNavs()).toEqual([]);
    press("Escape");
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    ctrlW();
    press("l");
    expect(paneNavs()).toHaveLength(1);
  });

  it("gives a composing key to the input method: Ctrl+w, then l while composing, moves nothing and spends the prefix", () => {
    browsing();
    ctrlW();
    press("l", { isComposing: true });
    press("l", { keyCode: 229 });
    expect(paneNavs()).toEqual([]);
    press("l");
    expect(paneNavs()).toEqual([]);
  });

  describe("the which-key box after Ctrl+w (200 ms, titled Ctrl+w)", () => {
    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());
    const box = (c: HTMLElement) => c.querySelector(".which-key-box");

    it("shows nothing before the delay, then the four moves under the title Ctrl+w, and clears on the next key", () => {
      const { container } = browsing();
      ctrlW();
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
      expect(box(container)).toBeNull();
      act(() => vi.advanceTimersByTime(1));
      expect(box(container)!.querySelector(".wk-title")!.textContent).toBe("Ctrl+w");
      expect(Array.from(box(container)!.querySelectorAll(".wk-key")).map((k) => k.textContent)).toEqual(["h", "j", "k", "l"]);
      expect(Array.from(box(container)!.querySelectorAll(".wk-entry")).map((e) => e.textContent)).toEqual([
        "h➜module left",
        "j➜module below",
        "k➜module above",
        "l➜module right",
      ]);
      press("k");
      expect(box(container)).toBeNull();
      expect(lastOfType("pane_nav")).toMatchObject({ direction: "up" });
    });

    it("Ctrl+w l inside the delay never shows it", () => {
      const { container } = browsing();
      ctrlW();
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
      press("l");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS * 3));
      expect(box(container)).toBeNull();
      expect(paneNavs()).toHaveLength(1);
    });

    it("a click on a row does what typing its key would", () => {
      const { container } = browsing();
      ctrlW();
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      const entry = Array.from(box(container)!.querySelectorAll(".wk-entry")).find((e) => e.textContent?.startsWith("j"))!;
      fireEvent.click(entry);
      expect(paneNavs()).toEqual([{ type: "pane_nav", request_id: expect.any(String), direction: "down" }]);
      expect(box(container)).toBeNull();
    });

    it("the g prefix's box keeps its own title, g", () => {
      const { container } = browsing();
      press("g");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(box(container)!.querySelector(".wk-title")!.textContent).toBe("g");
    });
  });
});

describe("the leader (panel round 2 plan, Task 8; spec 2026-09-26 §2)", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });
  function sendTable(panel: PanelTable = TABLE) {
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel, newTabChord: "Ctrl+b c" });
  }
  function tabVerbsPosted(): string[] {
    return posted.filter((m) => m.type === "tab_verb").map((m) => m.verb as string);
  }

  it("shows the box 200ms after Space, titled Space, with m/b/f in order", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press(" ");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS - 1));
      expect(container.querySelector(".which-key-box")).toBeNull();
      act(() => vi.advanceTimersByTime(1));
      const box = container.querySelector(".which-key-box")!;
      expect(box).not.toBeNull();
      expect(box.querySelector(".wk-title")!.textContent).toBe("Space");
      expect(Array.from(box.querySelectorAll(".wk-key")).map((k) => k.textContent)).toEqual(["m", "b", "f"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("Space b d typed within 50ms never shows the box, and closes the tab", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press(" ");
      act(() => vi.advanceTimersByTime(20));
      press("b");
      act(() => vi.advanceTimersByTime(20));
      press("d");
      expect(container.querySelector(".which-key-box")).toBeNull();
      expect(tabVerbsPosted()).toEqual(["close"]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 hardening R2-2: `H`/`L` (like `f`, below) are always the first key of whatever typed them,
   *  so they defer the same `TYPING_GUARD_MS` `a`/`d` do rather than running at once -- a lone `H`
   *  still runs, just `TYPING_GUARD_MS` later. `[ b`/`[ [` are a different path entirely (the
   *  reserved two-key prefix `resolveKey` itself owns, completed on the SECOND key), so they are
   *  untouched and still run at once. */
  it("H posts tab_verb prev TYPING_GUARD_MS later, [ b posts prev via the table at once, [ [ is still a prompt jump", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press("H");
      expect(tabVerbsPosted()).toEqual([]);
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(tabVerbsPosted()).toEqual(["prev"]);
      posted.length = 0;
      press("[");
      press("b");
      expect(tabVerbsPosted()).toEqual(["prev"]);
      posted.length = 0;
      press("[");
      press("[");
      expect(posted.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 hardening R2-2's own reproduction: two or more tabs open, on tab 2, `Ctrl+l` back to BROWSE
   *  and then "Looks good, now add tests⏎" typed at typing speed used to switch to tab 1 on the very
   *  first `L` (`mayActAfterMotion` cannot catch this: `L` is always the first key). Here the `o`
   *  that follows within `TYPING_GUARD_MS` cancels the deferred `L` before it ever posts, and the
   *  band says what did not happen in `L`'s own words -- the whole-branch review found it flashing
   *  the `a`/`d` text for every cancelled key, card or no card (`TypingGuard.defer`'s
   *  `cancelledFlash`). */
  it('"Looks good" at 80 ms a key posts no tab_verb, and the band names L, not a / d', () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = started();
      act(() => widen(container));
      sendTable();
      act(() => root(container).focus());
      for (const key of ["L", "o", "o", "k", "s"]) {
        press(key);
        act(() => vi.advanceTimersByTime(80));
      }
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(tabVerbsPosted()).toEqual([]);
      expect(container.querySelector(".band-message")?.textContent).toBe(tableKeyTypingFlash("L", "tab.next"));
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** `L` itself arriving soon after another key (rather than being cancelled by one that follows)
   *  refuses at once and says so with its own message: `k` (an ordinary move) then `L` within the
   *  window. */
  it("k then L at 80 ms a key: L refuses at once, and the band names it", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = started();
      act(() => widen(container));
      sendTable();
      act(() => root(container).focus());
      press("k");
      act(() => vi.advanceTimersByTime(80));
      press("L");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(tabVerbsPosted()).toEqual([]);
      expect(container.querySelector(".band-message")?.textContent).toBe(tableKeyTypingFlash("L", "tab.next"));
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** The same key alone after a pause still works: a real deliberate `L`, nothing else typed near
   *  it, runs `TYPING_GUARD_MS` later exactly as the fixed test above already pins for `H`. */
  it("a lone L still posts tab_verb next, just TYPING_GUARD_MS later", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press("L");
      expect(tabVerbsPosted()).toEqual([]);
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(tabVerbsPosted()).toEqual(["next"]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 audit P2-A6: `L`'s single-key binding defers `TYPING_GUARD_MS` (the test above); a `keymap`
   *  envelope arriving inside that window used to leave the deferred callback armed, closed over the
   *  OLD table's `binding` -- so replacing the table did not merely fail to run the NEW binding, it
   *  still ran the STALE one once the wait elapsed. `clearSequence` (Review Focus 1) only ever
   *  cancelled a pending multi-key sequence, never this. Codex's saved probe reproduces this exactly
   *  (`/scratch/v1-audit-probes/neovibe-p2/P2.audit.test.tsx`, "P2 drops a delayed table binding
   *  when the table is replaced"). */
  it("a table replaced mid-guard-window drops the deferred L binding entirely (P2-A6)", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press("L");
      expect(tabVerbsPosted()).toEqual([]);
      sendTable({
        ...TABLE,
        bindings: TABLE.bindings.map((b) => (b.keys.join("") === "L" ? { ...b, action: "tab.prev" as const } : b)),
      });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(tabVerbsPosted()).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** `H`/`L` use `defer` (both directions), not `mayActAfterMotion`: unlike the leader, a preceding
   *  run of motion keys does not exempt them -- the review's own reason not to reuse
   *  `mayActAfterMotion` here (there is no motion run behind the FIRST key of anything). */
  it("k k then H at 80 ms a key still defers (no motion-run exception, unlike the leader)", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press("k");
      act(() => vi.advanceTimersByTime(80));
      press("k");
      act(() => vi.advanceTimersByTime(80));
      press("H");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(tabVerbsPosted()).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 polish F16: vim's `gt`/`gT` (`:help gt`) step session tabs like `L`/`H`. `gT` arrives as a
   *  bare Shift keydown and then `T` with Shift held; the Shift must not drop the pending `g`. */
  it("g t posts next and g Shift T posts prev; a bare Shift keeps the pending g", () => {
    const { container } = started();
    sendTable({ ...TABLE, bindings: [...TABLE.bindings, binding(["g", "t"], "tab.next"), binding(["g", "T"], "tab.prev")] });
    act(() => root(container).focus());
    press("g");
    press("t");
    expect(tabVerbsPosted()).toEqual(["next"]);
    posted.length = 0;
    press("g");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Shift", shiftKey: true });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "T", shiftKey: true });
    expect(tabVerbsPosted()).toEqual(["prev"]);
    posted.length = 0;
    // `gg` is still the fixed jump, not a table key.
    press("g");
    press("g");
    expect(tabVerbsPosted()).toEqual([]);
  });

  /** v1 hardening, codex-release-p1 #6 (R25 "an unbound key after the leader is swallowed",
   *  leader.ts:76-84): a pending sequence's own keydown handler had no `isModifierKey` guard, unlike
   *  `pendingRef`'s `g`/`z`/`[`/`]` prefix (the test just above) and `TypingGuard.onKey`. The bare
   *  `Control` keydown of a `Ctrl+c` combo found no match, `advanceSequence` returned `cancel`, and
   *  the FOLLOWING keydown (`c`, `ctrlKey: true`) then saw `seqRef.current === null`, fell out of the
   *  leader block and reached `resolveKey`'s own `ctrlKey && event.key === "c"` arm -- interrupting a
   *  running turn instead of being swallowed as an unbound continuation. */
  it("a bare Ctrl keydown keeps a pending leader sequence alive, so Ctrl+c is swallowed rather than interrupting", () => {
    const { container } = started({ activeTurnId: "t1", capabilities: { ...initialState().capabilities, interrupt: true } });
    sendTable();
    act(() => root(container).focus());
    press(" "); // arms <leader>; TABLE's own bindings start with m/b/f, never c
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Control", ctrlKey: true });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "c", ctrlKey: true });
    expect(posted.some((m) => m.type === "interrupt")).toBe(false);
  });

  /** Fix round 1 (reviewer finding, codex-release-p1 #3): the #6 fix above keeps a bare Control
   *  keydown from cancelling the pending sequence, but the REAL character keydown that follows it
   *  (`d`, carrying `ctrlKey: true`) used to reach `advanceSequence` as plain "d", completing
   *  `<leader>bd` (tab.close) for a Ctrl+d chord no table entry actually names. */
  it("Space b Ctrl+d (the Control keydown included) does not run tab.close", () => {
    const { container } = started();
    sendTable();
    act(() => root(container).focus());
    press(" ");
    press("b");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Control", ctrlKey: true });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "d", ctrlKey: true });
    expect(tabVerbsPosted()).toEqual([]);
  });

  /** The control for the test above: with no modifier at all, Space b d still runs tab.close -- the
   *  fix narrows what a chord may complete, it does not touch the bare-key path. */
  it("Space b d (no modifier) still runs tab.close", () => {
    const { container } = started();
    sendTable();
    act(() => root(container).focus());
    press(" ");
    press("b");
    press("d");
    expect(tabVerbsPosted()).toEqual(["close"]);
  });

  it("Space with the Approve button focused starts no sequence, and no longer activates it (v1 S5)", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      events(
        { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: {} },
        { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
      );
      posted.length = 0;
      const approve = buttonLabelled(container, "Approve")!;
      approve.focus();
      // `fireEvent` returns false only when the handler called `preventDefault` (the same convention
      // the Approve-unreachable regression's own tests use). v1 S5 (spec 2026-09-27 §2.3): Space on a
      // card's answer button is claimed and does nothing -- not the button, not the leader.
      expect(fireEvent.keyDown(approve, { key: " " })).toBe(false);
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).toBeNull();
      expect(posted.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a composing Space starts nothing", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      posted.length = 0;
      fireEvent.keyDown(root(container), { key: " ", isComposing: true });
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).toBeNull();
      expect(posted.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a new keymap envelope cancels a pending sequence (Review Focus 1)", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      press(" ");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      sendTable();
      expect(container.querySelector(".which-key-box")).toBeNull();
      posted.length = 0;
      press("d");
      expect(posted.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("an ambiguous node runs at timeoutlen with no key", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      const ambiguous: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "b"], "tab.next")] };
      sendTable(ambiguous);
      act(() => root(container).focus());
      posted.length = 0;
      press(" ");
      press("b");
      act(() => vi.advanceTimersByTime(299));
      expect(posted.length).toBe(0);
      act(() => vi.advanceTimersByTime(1));
      expect(tabVerbsPosted()).toEqual(["next"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("with timeout: false, an ambiguous node never runs on its own", () => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      const noTimeout: PanelTable = {
        ...TABLE,
        timeout: false,
        bindings: [...TABLE.bindings, binding(["<leader>", "b"], "tab.next")],
      };
      sendTable(noTimeout);
      act(() => root(container).focus());
      posted.length = 0;
      press(" ");
      press("b");
      act(() => vi.advanceTimersByTime(10_000));
      expect(posted.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("pane_focus, a tab switch, hint_collect, ?, and i each cancel a pending sequence", () => {
    const { container } = started();
    sendTable();
    act(() => root(container).focus());

    function armThenCancel(cancel: () => void) {
      posted.length = 0;
      press(" ");
      cancel();
      press("b");
      press("d");
      expect(tabVerbsPosted()).toEqual([]);
      act(() => root(container).focus());
    }

    armThenCancel(() => dispatch({ kind: "pane_focus", focused: false }));
    armThenCancel(() => {
      dispatch({ kind: "tabs", active: 2, tabs: [{ ...LIVE_TAB, id: 2 }] });
      dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
    });
    armThenCancel(() => dispatch({ kind: "hint_collect", sessionId: 1 }));
    armThenCancel(() => press("?"));
    armThenCancel(() => press("i"));
  });

  /** The whole-branch review (spec §2.4's cancel list): an envelope that opens an overlay taking
   *  the keys ends a pending sequence, so a key the overlay lets bubble cannot advance a stale one. */
  it.each([
    ["chooser", { kind: "chooser", open: [], records: [] }],
    ["begin_rename", { kind: "begin_rename", tab: 1, current: null }],
    ["focus_permission", { kind: "focus_permission", tab: 1 }],
    ["confirm_close", { kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] }],
    ["confirm_close_others", { kind: "confirm_close_others", tabs: [2], lines: ["close 1 other tab? (y/n)"] }],
  ])("%s cancels a pending sequence", (_name, envelope) => {
    vi.useFakeTimers();
    try {
      const { container } = started();
      sendTable();
      act(() => root(container).focus());
      posted.length = 0;
      press(" ");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      dispatch(envelope);
      expect(container.querySelector(".which-key-box")).toBeNull();
      press("b");
      press("d");
      expect(tabVerbsPosted()).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 (D6): the wave-5 `SetPermissionMode` capability this used to gate on is gone -- a live
   *  session now always cycles, so `<leader>m` posts rather than flashing. */
  it("<leader>m cycles the mode on a live session", () => {
    const widen = stubBandWidth();
    const { container } = started();
    act(() => widen(container));
    sendTable();
    act(() => root(container).focus());
    press(" ");
    press("m");
    expect(container.querySelector(".band-message")).toBeNull();
    expect(posted.some((p) => p.type === "cycle_mode")).toBe(true);
    vi.unstubAllGlobals();
  });

  it("<leader>/ opens search, <leader>? opens the keymap, <leader>t posts nothing (Task 10 moves the handoff confirm)", () => {
    const withExtras: PanelTable = {
      ...TABLE,
      bindings: [
        ...TABLE.bindings,
        binding(["<leader>", "/"], "panel.search", "search"),
        binding(["<leader>", "?"], "panel.keymap", "keymap"),
        binding(["<leader>", "t"], "panel.handoff", "terminal"),
      ],
    };
    // Fake time, and a pause before each leader: the leader starts a sequence only on a key that
    // stands alone (the v1-ui GUI pass, `TypingGuard.mayActAfterMotion`), and three sequences fired
    // back to back in real time would read as typing.
    vi.useFakeTimers();
    try {
      const pause = () => act(() => vi.advanceTimersByTime(300));
      const { container } = started();
      sendTable(withExtras);
      act(() => root(container).focus());

      pause();
      press(" ");
      press("/");
      expect(container.querySelector(".search-bar")).not.toBeNull();
      act(() => root(container).focus());

      pause();
      press(" ");
      press("?");
      expect(container.querySelector(".keymap-overlay")).not.toBeNull();
      // Close it so it does not swallow the next press.
      fireEvent.keyDown(container.querySelector(".keymap-overlay")!, { key: "Escape" });
      act(() => root(container).focus());

      posted.length = 0;
      pause();
      press(" ");
      press("t");
      expect(posted.length).toBe(0);
      expect(container.querySelector(".handoff")).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

/* v1 S1, S4, S5 and F13 (spec `docs/superpowers/specs/2026-09-27-v1-ui-design.md` §2.1-§2.3): an
   arrival lands BROWSE with the cursor on a waiting card (R02, R32), and a Claude Code user types at
   once, so the first letters of a sentence arrive as BROWSE keys. `a`/`d`/`D` answer only when they
   stand alone (`./typingGuard`), only the cursor's card (S4), and Space on a card button does
   nothing (S5). Time is vitest's fake clock: jsdom stamps each keydown with `Date.now()`, so a gap
   here is exactly the gap between two keys. */
describe("v1: typing never answers a card", () => {
  const TYPING = "a / d answer a card only on their own — pause, then press again; i or Ctrl+j to type";
  const NO_CARD = "no card here — i, o, A or Ctrl+j to type";
  /** v1 hardening (R2-10): the same refusal while the card waits elsewhere points back to it. */
  const ELSEWHERE = "a / d answer the card under the cursor — j / k onto it, then a / d";
  let widen: (container: HTMLElement) => void;
  beforeEach(() => {
    vi.useFakeTimers();
    widen = stubBandWidth();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  /** A running turn with one card (gating `toolu_1`), then two rows below it, and the keys arrived
   *  on it the way `Ctrl+l`/`prefix a` bring them (`arrive`, which lands on the oldest card). */
  function arrivedOnACard() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    // `interrupt`, so the activity line has a Stop button: a button that answers no card.
    dispatchLiveTab(snapshotState({ capabilities: { ...initialState().capabilities, interrupt: true } }), 0);
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "meanwhile" },
      { type: "user_prompt_submitted", text: "and the docs" },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    expect(rendered.container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    return rendered;
  }
  /** One keydown wherever focus is, the way a real key arrives. */
  const press = (key: string, init: Record<string, unknown> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
  /** Enter or Space on a focused button, as a browser does it: jsdom never activates a button from
   *  a keydown (see "the Approve-unreachable regression" above), so the activation is modelled
   *  here -- a click exactly when the keydown's default was NOT prevented, the same way
   *  `typeIntoInput` models text insertion. Returns whether the default survived. */
  const pressOnButton = (key: string) => {
    const button = document.activeElement as HTMLElement;
    const notPrevented = fireEvent.keyDown(button, { key });
    if (notPrevented) fireEvent.click(button);
    return notPrevented;
  };
  const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
  const band = (container: HTMLElement) => container.querySelector(".band-message")?.textContent ?? null;
  const answered = () => posted.filter((m) => m.type === "permission_response");

  it("arrive, then a l s o at 80 ms a key: nothing is answered, and the band says how to type", () => {
    const { container } = arrivedOnACard();
    for (const key of ["a", "l", "s", "o"]) {
      press(key);
      wait(80);
    }
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(TYPING);
  });

  it("with no card waiting anywhere, a says there is no card here (F13)", () => {
    const { container } = arrivedOnACard();
    press("a");
    wait(1000);
    expect(answered()).toHaveLength(1);
    press("j");
    wait(300);
    press("a");
    expect(band(container)).toBe(NO_CARD);
  });

  /** Codex's whole-branch review: Rust refused the answer ("Always allow" with its rule unsavable)
   *  and left the card waiting, but this panel kept the id as answered for good -- `a`/`d` and the
   *  buttons stayed dead until a reset. A refusal now gives the card back. */
  it("a refused permission_response gives the card back: a and its buttons answer again", () => {
    const { container } = arrivedOnACard();
    press("a");
    wait(1000);
    const first = answered();
    expect(first).toHaveLength(1);
    const approve = () =>
      Array.from(container.querySelectorAll<HTMLButtonElement>(".permission-card button")).find(
        (b) => b.textContent === "Approve",
      )!;
    expect(approve().disabled).toBe(true);
    dispatch({
      kind: "command_result",
      requestId: first[0].request_id as string,
      ok: false,
      error: "could not save the rule: Permission denied",
    });
    expect(approve().disabled).toBe(false);
    press("a");
    wait(1000);
    expect(answered()).toHaveLength(2);
    expect(answered()[1]).toEqual(expect.objectContaining({ permission_id: "perm-1", decision: "allow" }));
  });

  it("a lone a approves the card under the cursor 250 ms later (R32 unchanged, only delayed)", () => {
    arrivedOnACard();
    press("a");
    wait(249);
    expect(answered()).toEqual([]);
    wait(1);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });

  it("a lone d denies it the same way", () => {
    arrivedOnACard();
    press("d");
    wait(250);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "deny" })]);
  });

  /* v1 audit P2-A1, ruling R2: before `KeyLike` gained `altKey`/`metaKey`, an Alt or Meta held
     alongside `a` reached `resolveKey`'s `case "a":` unchecked -- a common OS chord (Cmd/Ctrl+A
     "select all" muscle memory) that must never authorize a real tool call. Each modifier gets its
     own fresh card: two presses in the same test would have the second cancel the first's deferred
     answer regardless of this fix (`TypingGuard.onKey` cancels on every keydown), which would pass
     even with the bug still there. */
  it.each([{ altKey: true }, { metaKey: true }])(
    "a modified a is not permission approval: %j (v1 audit P2-A1)",
    (modifier) => {
      arrivedOnACard();
      press("a", modifier);
      wait(1000);
      expect(answered()).toEqual([]);
    },
  );

  /* Fix round 1 (v1 audit review, "R2's Super clause"): R2's own ruling names Super explicitly, and
     the fix that landed for Alt/Meta only ever checked `event.altKey`/`event.metaKey` -- a real
     `KeyboardEvent`'s `getModifierState("Super")` (jsdom honours `KeyboardEventInit`'s
     `modifierSuper`, and `fireEvent.keyDown` forwards it through) is the only signal this layer has
     for a physical Super/Hyper press; see `keymap.ts`'s own `KeyLike` doc comment for the honest
     caveat that this may still be a no-op on real WebKitGTK if it never surfaces Super as any DOM
     modifier at all. */
  it("a modified a is not permission approval: Super (v1 audit P2-A1, fix round 1)", () => {
    arrivedOnACard();
    press("a", { modifierSuper: true });
    wait(1000);
    expect(answered()).toEqual([]);
  });

  /* The sandbox pass (2026-09-28, Task 1) found the case above passing in jsdom and failing on real
     WebKitGTK 2.52.6, which never sets a modifier on `a` while Super is held: only Super's own
     keydown (`key: "Super"`, `code: "OSLeft"`) and keyup arrive. This is that shape. */
  it("a is not permission approval while Super is held, as WebKitGTK reports it (sandbox pass, 2026-09-28)", () => {
    arrivedOnACard();
    press("Super", { code: "OSLeft" });
    press("a");
    wait(1000);
    expect(answered()).toEqual([]);
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "Super", code: "OSLeft" });
    wait(300);
    press("a");
    wait(1000);
    expect(answered()).toHaveLength(1);
  });

  /* Codex re-review (2026-09-28): `shell/src/panel_super.rs` drops `Super+x` before the page, so
     after a waiting `a` the page sees only Super go down; that must cancel the answer. */
  it("a, then Super within the guard window: nothing is answered (the shell withholds the chord)", () => {
    arrivedOnACard();
    press("a");
    wait(100);
    press("Super", { code: "OSLeft" });
    wait(1000);
    expect(answered()).toEqual([]);
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "Super", code: "OSLeft" });
  });

  /* Fix round 2 (v1 audit review, "the AltGraph clause"): AltGr is a level-3 shift some layouts use
     to type an ordinary character, not one of the four modifiers R2 and fix round 1 named -- jsdom
     honours `KeyboardEventInit`'s `modifierAltGraph` the same way it honours `modifierSuper`. */
  it("a modified a is not permission approval: AltGraph (v1 audit fixes, finding 1)", () => {
    arrivedOnACard();
    press("a", { modifierAltGraph: true });
    wait(1000);
    expect(answered()).toEqual([]);
  });

  /* v1 audit P2-A2, ruling R3: Shift+Tab is claimed by `App.tsx`'s document-capture `onModeKey`
     ahead of the bubble-phase `onKeyDown` that would otherwise feed it to `typingGuard.onKey` --
     and `onModeKey` calls `event.stopPropagation()` on the very route (`cycle`) this reproduces, so
     that bubble handler never runs at all. Before R3's fix, a card answer deferred moments earlier
     stayed armed and still fired `TYPING_GUARD_MS` later even though the user had moved on. */
  it("Shift+Tab cancels a card answer deferred moments earlier (v1 audit P2-A2)", () => {
    const { container } = arrivedOnACard();
    press("a");
    wait(100);
    press("Tab", { code: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    // Fix round 1 (codex finding 2): the cancelled wait's own flash now shows, the same as any other
    // key cancelling a deferred `a`/`d` (`onModeKey` used to call a bare `typingGuard.cancel()`,
    // which drops the pending answer but discards its flash -- nothing told the user their `a` had
    // been discarded).
    expect(band(container)).toBe(TYPING);
    wait(TYPING_GUARD_MS);
    expect(answered()).toEqual([]);
  });

  /* Fix round 1 (v1 audit review, codex finding 2): before this fix, `onModeKey` cancelled a pending
     answer with a bare `typingGuard.cancel()`, which never records Shift+Tab as a key the guard has
     seen -- so a `a` pressed shortly AFTER a Shift+Tab (with nothing pending to cancel) skipped the
     guard's "before" half entirely and answered at once, unlike every other key (see "a key just
     before a refuses it at once" below, which is the same scenario with `x` in place of Shift+Tab).
     `typingGuard.onKey("Tab", ...)` closes it: Shift+Tab now counts as typing for the key that comes
     after it too. */
  it("Shift+Tab counts as typing for the guard's before-half too (v1 audit P2-A2, fix round 1)", () => {
    const { container } = arrivedOnACard();
    press("Tab", { code: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    wait(100);
    press("a");
    expect(band(container)).toBe(TYPING);
    wait(1000);
    expect(answered()).toEqual([]);
  });

  it("a then j at 200 ms: the j cancels the answer, flashes, and still moves", () => {
    const { container } = arrivedOnACard();
    press("a");
    wait(200);
    press("j");
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(TYPING);
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(false);
  });

  it("a key just before a refuses it at once, and says so", () => {
    const { container } = arrivedOnACard();
    press("x");
    wait(100);
    press("a");
    expect(band(container)).toBe(TYPING);
    wait(1000);
    expect(answered()).toEqual([]);
  });

  it("a held a's repeat answers nothing", () => {
    arrivedOnACard();
    press("a", { repeat: true });
    wait(1000);
    expect(answered()).toEqual([]);
  });

  it.each([
    ["pane_focus false", () => dispatch({ kind: "pane_focus", focused: false })],
    ["the ? overlay opening", () => dispatch({ kind: "open_keymap" })],
    ["a switch to another tab", () => dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }] })],
    [
      "the card resolving",
      () =>
        dispatch({
          kind: "events", tab: 1, fromRevision: 6, throughRevision: 7,
          events: [{ type: "permission_resolved", permission_id: "perm-1", outcome: "allowed" }],
        }),
    ],
  ])("a lone a, then %s before 250 ms: nothing is answered", (_name, interrupt) => {
    arrivedOnACard();
    press("a");
    wait(100);
    interrupt();
    wait(1000);
    expect(answered()).toEqual([]);
  });

  it("two rows below a lone card, a answers nothing and points back to the card (S4, F13, R2-10)", () => {
    const { container } = arrivedOnACard();
    press("j");
    press("j");
    expect(container.querySelector(".row-current")!.textContent).toContain("and the docs");
    wait(300);
    press("a");
    expect(band(container)).toBe(ELSEWHERE);
    wait(1000);
    expect(answered()).toEqual([]);
  });

  it("Space on a focused Approve does nothing: not the button, not the leader (S5)", () => {
    const { container } = arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    press("l");
    expect(document.activeElement?.textContent).toBe("Approve");
    wait(300);
    expect(pressOnButton(" ")).toBe(false);
    wait(WHICH_KEY_DELAY_MS + 1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".which-key-box")).toBeNull();
    expect(document.activeElement?.textContent).toBe("Approve");
  });

  /* K02: Enter on a card's own button is claimed, and the panel presses the button itself
     TYPING_GUARD_MS later (`a`/`d`'s own wait) -- so these three read the answer after it. */
  it("Enter on a focused Approve after 300 ms idle approves", () => {
    arrivedOnACard();
    press("l");
    wait(300);
    expect(pressOnButton("Enter")).toBe(false);
    wait(249);
    expect(answered()).toEqual([]);
    wait(1);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });

  it("l then Enter at 100 ms still approves: l is the walk onto the button", () => {
    arrivedOnACard();
    press("l");
    wait(100);
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });

  it("e then Enter at 100 ms on Approve answers nothing, and says so", () => {
    const { container } = arrivedOnACard();
    press("l");
    wait(300);
    press("e");
    wait(100);
    expect(pressOnButton("Enter")).toBe(false);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(TYPING);
  });

  /* Fix round 1: a sentence ending in `l` walks onto Approve (the card's first control), and the
     walk exception once let its Enter through. Every key of the unbroken run before Enter must now be
     a walk (`./typingGuard`), so the spec's "…al⏎ lands on Deny" residual is refused instead. */
  it.each([["cancel"], ["al"], ["deal"]])("arrive, then %s and Enter at 80 ms a key: Approve is focused and nothing is answered", (word) => {
    const { container } = arrivedOnACard();
    for (const key of word) {
      press(key);
      wait(80);
    }
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(TYPING);
  });

  it("l l then Enter at 80 ms a key walks onto Deny and denies", () => {
    arrivedOnACard();
    press("l");
    wait(80);
    press("l");
    wait(80);
    expect(document.activeElement?.textContent).toBe("Deny");
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "deny" })]);
  });

  /* The fire-time check in `waitThenAnswer`: a card that resolved during the wait is never pressed,
     even though its button object is still held. React never sees a click on a detached button, so
     a native listener on the button itself is what shows the press did not happen. */
  it("a lone a, then the card resolving: its Approve button is never pressed", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    const pressed = vi.fn();
    approve.addEventListener("click", pressed);
    press("a");
    wait(100);
    dispatch({
      kind: "events", tab: 1, fromRevision: 6, throughRevision: 7,
      events: [{ type: "permission_resolved", permission_id: "perm-1", outcome: "allowed" }],
    });
    expect(approve.isConnected).toBe(false);
    wait(1000);
    expect(pressed).not.toHaveBeenCalled();
    expect(answered()).toEqual([]);
  });

  it("Enter on a button that answers nothing (Stop) keeps the browser's own activation", () => {
    arrivedOnACard();
    press("x");
    const stop = buttonLabelled(document.body, "ctrl+c interrupt")!;
    act(() => stop.focus());
    // A key on its own (the v1-ui GUI pass: Stop no longer acts in the middle of typing, below).
    wait(300);
    expect(fireEvent.keyDown(stop, { key: "Enter" })).toBe(true);
    wait(300);
    expect(fireEvent.keyDown(stop, { key: " " })).toBe(true);
  });

  /* The v1-ui GUI pass (2026-09-27): `j` from the last row lands on Stop, so "just do it" typed after
     an arrival interrupted the turn with its first Space (the card was denied with it), and "jl⏎"
     the same with its Enter. Stop now acts only on a key that stands alone or ends a quick motion. */
  it.each([[" "], ["Enter"]])("t then %j on Stop at 80 ms interrupts nothing, and says so", (key) => {
    const { container } = arrivedOnACard();
    const stop = buttonLabelled(document.body, "ctrl+c interrupt")!;
    act(() => stop.focus());
    wait(300);
    press("t");
    wait(80);
    expect(pressOnButton(key)).toBe(false);
    expect(posted.some((m) => m.type === "interrupt")).toBe(false);
    expect(band(container)).toBe("Stop takes a key only on its own — i or Ctrl+j to type");
  });

  it("j onto Stop then Enter at 80 ms is one quick motion: it interrupts", () => {
    arrivedOnACard();
    const stop = buttonLabelled(document.body, "ctrl+c interrupt")!;
    act(() => stop.focus());
    wait(300);
    press("j");
    wait(80);
    expect(pressOnButton("Enter")).toBe(true);
    expect(posted.some((m) => m.type === "interrupt")).toBe(true);
  });

  /* The same pass: the leader starts a sequence wherever the row cursor has the keys, so typed prose
     ran them -- "set up my" `<leader>m` (the mode switch that, on a switch-capable sidecar, approves
     every waiting card), "the boy" `<leader>bo` and its own `y`. */
  it("e Space b b at 80 ms a key: no leader sequence runs, and the band says so", () => {
    const { container } = arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    for (const key of ["e", " ", "b", "b"]) {
      press(key);
      wait(80);
    }
    wait(1000);
    expect(posted.filter((m) => m.type === "tab_verb")).toEqual([]);
    expect(container.querySelector(".which-key-box")).toBeNull();
    expect(band(container)).toBe("Space starts a sequence only on its own — i or Ctrl+j to type");
  });

  it("t Space m at 80 ms a key: <leader>m never runs", () => {
    const { container } = arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    const before = posted.length;
    for (const key of ["t", " ", "m"]) {
      press(key);
      wait(80);
    }
    wait(1000);
    expect(posted.slice(before).filter((m) => m.type === "cycle_mode" || m.type === "cycle_default_mode")).toEqual([]);
    expect(band(container)).not.toMatch(/mode is fixed/);
  });

  it("k k then Space b b at 80 ms a key: a quick motion, so the leader still runs", () => {
    arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    for (const key of ["k", "k", " ", "b", "b"]) {
      press(key);
      wait(80);
    }
    expect(posted.filter((m) => m.type === "tab_verb").map((m) => m.verb)).toEqual(["last"]);
  });

  it("a lone D puts the keys in the card's reason box 250 ms later", () => {
    const { container } = arrivedOnACard();
    press("D", { shiftKey: true });
    const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
    expect(document.activeElement).not.toBe(reason);
    wait(250);
    expect(document.activeElement).toBe(reason);
  });

  it("D then o within 250 ms: the reason box is not focused", () => {
    const { container } = arrivedOnACard();
    press("D", { shiftKey: true });
    wait(100);
    press("o");
    wait(1000);
    expect(document.activeElement).not.toBe(container.querySelector(".permission-card input"));
    expect(band(container)).toBe(TYPING);
  });

  /* K01 (kbux 2026-09-29, re-verified 2/2): a prefix, a pause past every timer, then an answer key. */
  it.each([["g", "d"], ["g", "a"], ["]", "d"], ["[", "d"], ["z", "d"], ["g", "D"]])("K01: %s, a pause, then %s answers nothing", (first, second) => {
    const { container } = arrivedOnACard();
    wait(1000);
    press(first);
    wait(5000); // past WHICH_KEY_DELAY_MS, TYPING_GUARD_MS and any timeoutlen
    press(second, second === "D" ? { shiftKey: true } : {});
    wait(5000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".which-key-box")).toBeNull();
    expect(document.activeElement?.closest(".permission-card input") ?? null).toBeNull();
  });
  it("K01: a count, a pause, then a answers nothing and says why", () => {
    const { container } = arrivedOnACard();
    wait(1000); press("3"); wait(5000); press("a");
    // Read at once: a flash lasts 2 s (`showFlash`'s own timer), so 5 s on the band is empty again.
    expect(band(container)).toBe("a / d / D take no count — press it on its own");
    wait(5000);
    expect(answered()).toEqual([]);
  });
  it("K01: g, a pause, then Enter on a focused Approve presses nothing", () => {
    arrivedOnACard();
    press("l"); wait(1000); press("g"); wait(5000);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
  });
  it("K01: g then a held d never answers, and a lone a after a cancelled g still does", () => {
    arrivedOnACard();
    wait(1000); press("g"); wait(1000); press("d");
    for (let i = 0; i < 3; i++) press("d", { repeat: true });
    wait(1000);
    expect(answered()).toEqual([]);
    press("a"); wait(1000);
    expect(answered()).toHaveLength(1);
  });
  it("K01: g then a key an input method is composing drops the prefix and answers nothing", () => {
    arrivedOnACard();
    wait(1000); press("g"); wait(1000); press("d", { isComposing: true }); press("d", { keyCode: 229 }); wait(1000);
    expect(answered()).toEqual([]);
  });
  /* v1 picks, Task 4: `za` is a pair `z` completes now (toggle a fold, as Enter does), so the `a` after
     a `z` is never the card's answer -- typed at once or after a pause -- and it spends the prefix: a
     lone `a` afterwards still answers, 250 ms later. The cursor's row here is the card itself, which
     folds nothing. */
  it.each([[80], [5000]])("Task 4: z, %i ms, then a toggles a fold and answers nothing; a lone a after it still answers", (gap) => {
    const { container } = arrivedOnACard();
    wait(1000); press("z"); wait(gap); press("a"); wait(5000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".which-key-box")).toBeNull();
    press("a"); wait(1000);
    expect(answered()).toHaveLength(1);
  });
  /* Review Focus 2 for Task 4's own pair: `za` completes, so the `a` a held key repeats afterwards
     arrives as the card-answer key -- and a repeat never answers (S1). */
  it("Task 4: z then a held a toggles a fold once and never answers", () => {
    arrivedOnACard();
    wait(1000); press("z"); wait(1000); press("a");
    for (let i = 0; i < 3; i++) press("a", { repeat: true });
    wait(1000);
    expect(answered()).toEqual([]);
    press("a"); wait(1000);
    expect(answered(), "and a lone a after the hold still answers").toHaveLength(1);
  });
  it.each([["o"], ["c"], ["t"], ["z"], ["b"]])("Task 4: z then %s on a waiting card answers nothing and leaves it waiting", (second) => {
    const { container } = arrivedOnACard();
    wait(1000); press("z"); wait(1000); press(second); wait(5000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    expect(container.querySelector(".permission-card")).not.toBeNull();
  });
  /* v1 picks, Task 5: a page (Ctrl+f, PageDown, PageUp) and the arrow keys only scroll or move the
     cursor -- pressed on a waiting card, at once or after a pause, they answer nothing and leave it
     waiting; and, as keys, they cancel an answer deferred just before them like any other key (S1). */
  const PAGE_AND_ARROW_KEYS: Array<[string, string, Record<string, unknown>]> = [
    ["Ctrl+f", "f", { ctrlKey: true }],
    ["PageDown", "PageDown", {}],
    ["PageUp", "PageUp", {}],
    ["ArrowDown", "ArrowDown", {}],
    ["ArrowUp", "ArrowUp", {}],
  ];
  it.each(PAGE_AND_ARROW_KEYS)("Task 5: %s on a waiting card answers nothing, at once or after a pause", (_name, k, init) => {
    const { container } = arrivedOnACard();
    wait(1000); press(k, init); wait(5000); press(k, init); wait(10); press(k, init); wait(5000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".permission-card")).not.toBeNull();
    expect(posted.some((m) => m.type === "interrupt")).toBe(false);
  });
  it.each(PAGE_AND_ARROW_KEYS)("Task 5: a, then %s within the guard window cancels the answer", (_name, k, init) => {
    const { container } = arrivedOnACard();
    wait(1000); press("a"); wait(100); press(k, init);
    expect(band(container), "the cancelled wait says so").toBe(TYPING);
    wait(5000);
    expect(answered()).toEqual([]);
  });
  /* The other order: a key just BEFORE `a` -- an arrow the reader used to look around -- makes the `a`
     one of a run, so it answers nothing either (the before-half of S1). Only a lone `a` answers. */
  it.each(PAGE_AND_ARROW_KEYS)("Task 5: %s, then a within the guard window answers nothing", (_name, k, init) => {
    arrivedOnACard();
    wait(1000); press(k, init); wait(80); press("a"); wait(5000);
    expect(answered()).toEqual([]);
  });
  /* And Enter on a button `l` put focus on (K02, R3): the landing has to be the key right before the
     Enter. A page leaves focus on Approve while the card's row stays in view (this list has no layout,
     so nothing scrolls it away), and an arrow moves it off; either is a key in between, so the Enter
     presses nothing however long it waits. */
  it.each(PAGE_AND_ARROW_KEYS)("Task 5: l onto Approve, then %s, then Enter answers nothing", (_name, k, init) => {
    arrivedOnACard();
    press("l"); wait(400);
    expect(document.activeElement?.textContent).toBe("Approve");
    press(k, init); wait(400);
    pressOnButton("Enter"); wait(5000);
    expect(answered()).toEqual([]);
  });
  /* Fix round 1 (review): the rest of the K01 Enter/Space rule. While a prefix waits, Enter/Space
     on ANY focused control is swallowed (R1) -- Stop included, whose press interrupts the turn and
     denies the waiting card with it; a count before Enter/Space on a card's own button cancels, and
     says why (R2). */
  it.each([["Enter"], [" "]])("K01: g, a pause, then %j on Stop interrupts nothing", (key) => {
    arrivedOnACard();
    const stop = buttonLabelled(document.body, "ctrl+c interrupt")!;
    act(() => stop.focus());
    wait(1000); press("g"); wait(5000);
    expect(document.activeElement).toBe(stop);
    expect(pressOnButton(key)).toBe(false);
    wait(1000);
    expect(posted.some((m) => m.type === "interrupt")).toBe(false);
    expect(answered()).toEqual([]);
  });
  it.each([["Enter"], [" "]])("K01: a count, a pause, then %j on a focused Approve answers nothing and says why", (key) => {
    const { container } = arrivedOnACard();
    press("l"); wait(1000); press("3"); wait(5000);
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(pressOnButton(key)).toBe(false);
    expect(band(container)).toBe("a / d / D take no count — press it on its own");
    wait(1000);
    expect(answered()).toEqual([]);
  });
  /* Fix round 1 (review): a prefix's second key never starts a leader sequence (R1), so `g`, a
     pause, Space is a cancelled `g`, and the `m` after it is a plain `m` -- never `<leader>m`. */
  it("K01: g, a pause, then Space starts no leader sequence: g Space m never reaches <leader>m", () => {
    const { container } = arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    wait(1000); press("g"); wait(5000); press(" "); wait(1000);
    expect(container.querySelector(".which-key-box")).toBeNull();
    press("m"); wait(1000);
    expect(posted.filter((m) => m.type === "cycle_mode")).toEqual([]);
    expect(answered()).toEqual([]);
  });

  /* v1 picks, Task 6 (R11): `Ctrl+w` is a reserved prefix, so the K01 rule holds for it as for `g`: the
     key after it completes h/j/k/l or is swallowed -- never the card's answer, at once or after a
     pause. Its own pairs move focus to another module and answer nothing either. */
  it.each([["a", {}], ["d", {}], ["D", { shiftKey: true }]])("Task 6: Ctrl+w, a pause, then %s answers nothing", (second, init) => {
    const { container } = arrivedOnACard();
    wait(1000);
    press("w", { ctrlKey: true });
    wait(5000); // past WHICH_KEY_DELAY_MS, TYPING_GUARD_MS and any timeoutlen
    press(second, init);
    wait(5000);
    expect(answered()).toEqual([]);
    expect(posted.filter((m) => m.type === "pane_nav")).toEqual([]);
    expect(container.querySelector(".which-key-box")).toBeNull();
    expect(document.activeElement?.closest(".permission-card input") ?? null).toBeNull();
    expect(container.querySelector(".permission-card")).not.toBeNull();
  });
  it("Task 6: Ctrl+w then a held d never answers, and a lone a after a cancelled Ctrl+w still does", () => {
    arrivedOnACard();
    wait(1000); press("w", { ctrlKey: true }); wait(1000); press("d");
    for (let i = 0; i < 3; i++) press("d", { repeat: true });
    wait(1000);
    expect(answered()).toEqual([]);
    press("a"); wait(1000);
    expect(answered()).toHaveLength(1);
  });
  it("Task 6: Ctrl+w then a key an input method is composing drops the prefix and answers nothing", () => {
    arrivedOnACard();
    wait(1000); press("w", { ctrlKey: true }); wait(1000);
    press("d", { isComposing: true }); press("d", { keyCode: 229 }); wait(1000);
    press("l"); wait(1000);
    expect(answered()).toEqual([]);
    expect(posted.filter((m) => m.type === "pane_nav")).toEqual([]);
  });
  /* The pairs themselves: a pane move on a waiting card posts `pane_nav` and nothing else, and (as
     keys) they are neighbours to the guard like any other -- an `a` right before or after one is one of
     a run and answers nothing (S1), while a lone `a` after a pause still does. */
  it("Task 6: Ctrl+w l on a waiting card moves the keys away and answers nothing", () => {
    const { container } = arrivedOnACard();
    wait(1000); press("w", { ctrlKey: true }); press("l"); wait(5000);
    expect(posted.filter((m) => m.type === "pane_nav")).toEqual([expect.objectContaining({ direction: "right" })]);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".permission-card")).not.toBeNull();
    press("a"); wait(1000);
    expect(answered()).toHaveLength(1);
  });
  it("Task 6: a, then Ctrl+w within the guard window cancels the answer", () => {
    const { container } = arrivedOnACard();
    wait(1000); press("a"); wait(100); press("w", { ctrlKey: true });
    expect(band(container), "the cancelled wait says so").toBe(TYPING);
    wait(5000);
    expect(answered()).toEqual([]);
  });
  it("Task 6: Ctrl+w j, then a within the guard window answers nothing (the before-half of S1)", () => {
    arrivedOnACard();
    wait(1000); press("w", { ctrlKey: true }); press("j"); wait(80); press("a"); wait(5000);
    expect(answered()).toEqual([]);
  });
  /* K02's landing has to be the key right before Enter: `l` onto Approve, then a whole Ctrl+w pair, then
     Enter presses nothing, however long it waits -- and so does a Ctrl+w left waiting before the Enter. */
  it.each([["j"], ["h"]])("Task 6: l onto Approve, then Ctrl+w %s, then Enter answers nothing", (second) => {
    arrivedOnACard();
    press("l"); wait(400);
    expect(document.activeElement?.textContent).toBe("Approve");
    press("w", { ctrlKey: true }); press(second); wait(400);
    pressOnButton("Enter"); wait(5000);
    expect(answered()).toEqual([]);
  });
  it("Task 6: l onto Approve, Ctrl+w, a pause, then Enter on the focused Approve presses nothing", () => {
    arrivedOnACard();
    press("l"); wait(1000); press("w", { ctrlKey: true }); wait(5000);
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
  });

  /* K02 (kbux 2026-09-29: `:ls⏎` approved `rm -rf important`; ruling R3): a card's own button
     decides only the way `a`/`d` do. Enter there is claimed, and the panel presses the button itself
     `TYPING_GUARD_MS` later, only with S1's before-half, no modifier held, and focus put on that very
     button by the key right before Enter (`h`/`l` moving it, a Tab) or by a HINT landing with no key
     since. `pressOnButton` models the browser's own activation, so an answer read after it returned
     false is the panel's own deferred press. */
  const ENTER_LANDING = "Enter answers a card right after l, h or Tab onto its button";
  it("K02: l, s, Enter at 400 ms a key approves nothing (the walk did not land the Enter)", () => {
    const { container } = arrivedOnACard();
    wait(1000); press("l"); wait(400); press("s"); wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });
  it("K02: :ls Enter at 400 ms a key opens a command line and answers nothing", () => {
    const { container } = arrivedOnACard();
    wait(400); press(":", { shiftKey: true });
    wait(400); press("l");
    wait(400); press("s");
    wait(400);
    // A browser's own activation: a focused button clicks when Enter's default survives.
    const target = document.activeElement as HTMLElement;
    if (fireEvent.keyDown(target, { key: "Enter" }) && target.tagName === "BUTTON") fireEvent.click(target);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".search-bar")).toBeNull();
  });
  it("K02: l onto Approve, then :ls Enter at 400 ms a key: the line takes them, and nothing is answered", () => {
    const { container } = arrivedOnACard();
    press("l");
    expect(document.activeElement?.textContent).toBe("Approve");
    for (const key of [":", "l", "s"]) {
      wait(400);
      press(key, key === ":" ? { shiftKey: true } : {});
    }
    wait(400);
    const target = document.activeElement as HTMLElement;
    expect(target.tagName).toBe("INPUT");
    if (fireEvent.keyDown(target, { key: "Enter" }) && target.tagName === "BUTTON") fireEvent.click(target);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".search-bar")).toBeNull();
  });
  it("K02: l onto Approve, then g h (a cancelled pair, no focus move) and Enter at 400 ms, approves nothing", () => {
    arrivedOnACard();
    press("l"); wait(400); press("g"); wait(400); press("h"); wait(400);
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
  });
  it("K02: Tab onto Approve, then Enter, approves after S1's wait", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    wait(400); press("Tab"); act(() => approve.focus()); // the browser's own Tab move
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });
  it("K02: a Tab whose move left the page is no landing when focus comes back onto Approve", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    wait(400); press("Tab");
    // The browser's own Tab move went to another widget (focus left this page), then came back.
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    act(() => approve.focus());
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });
  it("K02: focus no key put on Approve is no landing: Tab, x, then Approve focused from elsewhere", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    wait(400); press("Tab"); wait(400); press("x"); act(() => approve.focus());
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });
  it("K02: h from Deny back onto Approve, then Enter, approves after S1's wait", () => {
    arrivedOnACard();
    press("l"); press("l");
    expect(document.activeElement?.textContent).toBe("Deny");
    wait(300); press("h");
    expect(document.activeElement?.textContent).toBe("Approve");
    wait(300);
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });
  it("K02: Shift+, Alt+, Ctrl+ and Meta+Enter on a focused Approve answer nothing", () => {
    const { container } = arrivedOnACard();
    press("l"); wait(300);
    const approve = document.activeElement as HTMLElement;
    for (const mod of [{ shiftKey: true }, { altKey: true }, { ctrlKey: true }, { metaKey: true }]) {
      if (fireEvent.keyDown(approve, { key: "Enter", ...mod })) fireEvent.click(approve);
      expect(band(container), JSON.stringify(mod)).toBe("Enter with a modifier answers no card");
      wait(1000);
    }
    expect(answered()).toEqual([]);
  });
  it("K02: a held Enter on a landed Approve: its repeat cancels the press, and nothing is answered", () => {
    arrivedOnACard();
    press("l"); wait(300);
    expect(pressOnButton("Enter")).toBe(false);
    for (let i = 0; i < 3; i++) {
      wait(30);
      // A repeat's default is claimed too, so the browser never activates the button either.
      expect(fireEvent.keyDown(document.activeElement!, { key: "Enter", repeat: true })).toBe(false);
    }
    wait(1000);
    expect(answered()).toEqual([]);
  });
  it.each([
    ["pane_focus false", () => dispatch({ kind: "pane_focus", focused: false })],
    ["the ? overlay opening", () => dispatch({ kind: "open_keymap" })],
    ["a switch to another tab", () => dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }] })],
    [
      "the card resolving",
      () =>
        dispatch({
          kind: "events", tab: 1, fromRevision: 6, throughRevision: 7,
          events: [{ type: "permission_resolved", permission_id: "perm-1", outcome: "allowed" }],
        }),
    ],
  ])("K02: l, Enter on Approve, then %s before 250 ms: nothing is pressed", (_name, interrupt) => {
    arrivedOnACard();
    press("l"); wait(300);
    expect(pressOnButton("Enter")).toBe(false);
    wait(100);
    interrupt();
    wait(1000);
    expect(answered()).toEqual([]);
  });
  /* K02 fix round 3 (review): the switch above is caught by the button leaving the page. Here the new
     tab's card sits at the same `p-<seq>` key and the switch and its snapshot land in one commit, as
     one batch from Rust can, so React keeps the very button element -- connected and enabled, now
     drawing the NEW tab's card -- and `isConnected` cannot stop the press. Three guards do: the
     switch's own `typingGuard.cancel()`, the fire-time tab check behind it, and the switch landing the
     keys on the root (focus leaves the button). The tab check is never the only one: `activeTabRef`
     changes only in the `tabs` arm, which cancels the wait in the same branch. */
  it("K02: l, Enter on Approve, then a switch to a tab whose card keeps the very same button: nothing is pressed", () => {
    const { container } = arrivedOnACard();
    press("l"); wait(300);
    const approve = document.activeElement as HTMLButtonElement;
    expect(approve.textContent).toBe("Approve");
    expect(pressOnButton("Enter")).toBe(false);
    wait(100);
    const other: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t9" },
      { type: "tool_call_started", turn_id: "t9", tool_use_id: "toolu_9", name: "Bash", input: { command: "rm build" } },
      { type: "permission_requested", permission_id: "perm-9", tool_use_id: "toolu_9", tool_name: "Bash", input: { command: "rm build" } },
      { type: "content_delta", turn_id: "t9", kind: "text", text: "meanwhile" },
      { type: "user_prompt_submitted", text: "and the docs" },
    ];
    const state = other.reduce(applyEvent, snapshotState({ capabilities: { ...initialState().capabilities, interrupt: true } }));
    act(() => {
      window.__neovibeDispatch!(
        JSON.stringify({ kind: "tabs", active: 2, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 live" }] }),
      );
      window.__neovibeDispatch!(JSON.stringify({ kind: "snapshot", tab: 2, throughRevision: other.length, state }));
    });
    expect(approve.isConnected).toBe(true);
    expect(container.querySelector('.permission-card [data-nav-action="allow"]')).toBe(approve);
    wait(1000);
    expect(answered()).toEqual([]);
  });
  /* The `/` and `:` lines take their own Enter and Esc (`SearchBar` stops them there), so the guard
     used to miss them: `:ls`, a pause, Enter, then `a` at once answered the cursor's card as a key
     standing alone. */
  it.each([
    [":", "Enter"],
    [":", "Escape"],
    ["/", "Enter"],
    ["/", "Escape"],
  ])("K02: %s, then %s in its line, then a at 80 ms answers nothing (the line's own key counts)", (lead, closeKey) => {
    const { container } = arrivedOnACard();
    wait(400);
    press(lead, lead === ":" ? { shiftKey: true } : {});
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    expect(document.activeElement).toBe(input);
    wait(400);
    fireEvent.keyDown(input, { key: closeKey });
    expect(container.querySelector(".search-bar")).toBeNull();
    wait(80);
    press("a");
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(TYPING);
  });
  it("K02: the : line closes silently on Esc and on pane_focus, and its Enter says what it is", () => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    const line = () => container.querySelector<HTMLInputElement>(".search-bar input");
    press(":", { shiftKey: true });
    expect(line()?.getAttribute("aria-label")).toBe("Command line");
    expect(container.querySelector(".search-bar")!.textContent).toBe(":");
    expect(document.activeElement).toBe(line());
    fireEvent.keyDown(line()!, { key: "Escape" });
    expect(line()).toBeNull();
    expect(band(container)).toBeNull();
    expect(document.activeElement).toBe(root);
    wait(300);
    press(":", { shiftKey: true });
    fireEvent.change(line()!, { target: { value: "ls" } });
    fireEvent.keyDown(line()!, { key: "Enter" });
    expect(line()).toBeNull();
    expect(band(container)).toBe(":ls — no ex commands here; ? lists this panel's keys");
    expect(document.activeElement).toBe(root);
    wait(300);
    press(":", { shiftKey: true });
    expect(line()).not.toBeNull();
    dispatch({ kind: "pane_focus", focused: false });
    expect(line()).toBeNull();
  });

  /* K02 fix round 1 (review): two guards no test above reached. The press is checked again when it
     fires, so focus moved off Approve by no key (a click elsewhere) within the wait presses nothing;
     and a Tab's landing is used up by the first focus after it, so a second focus no key made is no
     landing. */
  it("K02: l, Enter, then focus moved off Approve by no key before 250 ms: nothing is pressed", () => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    press("l");
    wait(300);
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(pressOnButton("Enter")).toBe(false);
    wait(100);
    act(() => root.focus()); // a click on the conversation: no key, so the wait itself goes on
    wait(1000);
    expect(answered()).toEqual([]);
  });
  it("K02: a Tab's landing is used up by the first focus after it: a second focus no key made is no landing", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    const stop = container.querySelector<HTMLButtonElement>(".activity-line .stop")!;
    wait(400);
    press("Tab");
    act(() => stop.focus()); // the browser's own Tab move, onto Stop
    act(() => approve.focus()); // then a focus no key made (a click, an effect)
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });

  /* K02 fix round 1 (review; there before K02): the chooser and the /model picker return from
     `onKeyDown` ahead of the card-button rules, so Enter or Space on a card button that still had
     focus under either -- a mouse pressed on Approve and dragged off it, so no click -- was the
     browser's own activation. Both overlays are modal: those keys on a card's own buttons are
     claimed there too now, and do nothing. */
  const MODEL_REPLY =
    "Current model: `Haiku 4.5` (effort: high)\nUsage: /model <name>. Available: sonnet, opus, haiku, or a full model ID.";
  /** A live tab where a bare `/model` was sent and its reply opened the picker, with a card waiting. */
  function pickerOverACard() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    enterInputMode(rendered.container);
    const textarea = rendered.container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "/model" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    const list: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    expect(rendered.container.querySelector(".slash-picker")).not.toBeNull();
    return rendered;
  }
  it.each([
    ["the chooser", ".chooser", () => {
      const rendered = arrivedOnACard();
      dispatch({ kind: "chooser", open: [], records: [] });
      return rendered;
    }],
    ["the /model picker", ".slash-picker", pickerOverACard],
  ])("K02: with %s open, Enter or Space on a card button that still has focus answers nothing", (_name, selector, setUp) => {
    const { container } = setUp();
    expect(document.activeElement).toBe(container.querySelector(selector));
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    act(() => approve.focus()); // a mouse pressed on Approve and dragged off it: focus, no click
    for (const key of ["Enter", " "]) {
      wait(400);
      expect(pressOnButton(key), JSON.stringify(key)).toBe(false);
      wait(1000);
    }
    expect(answered()).toEqual([]);
    expect(container.querySelector(selector)).not.toBeNull();
  });

  /* K02 fix round 1 (review): the `/` prompt and the `:` line are one command line, as vim's are --
     opening either closes the other, so no second box is ever drawn without the keys, and a keyboard
     arrival (`takeKeys`) hands the keys to the one on screen. */
  it.each([
    ["/", ":", "Command line"],
    [":", "/", "Search the conversation"],
  ])("K02: %s open, the keys back on the conversation, then %s: one line, the new one, holding the keys", (first, second, label) => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    press(first, first === ":" ? { shiftKey: true } : {});
    expect(container.querySelectorAll(".search-bar")).toHaveLength(1);
    act(() => root.focus()); // a click back on the conversation
    press(second, second === ":" ? { shiftKey: true } : {});
    const lines = container.querySelectorAll<HTMLElement>(".search-bar");
    expect(lines).toHaveLength(1);
    const input = lines[0].querySelector("input")!;
    expect(input.getAttribute("aria-label")).toBe(label);
    expect(document.activeElement).toBe(input);
    act(() => root.focus());
    dispatch({ kind: "arrive" });
    expect(document.activeElement).toBe(input);
  });
  it("K02: the : line open, the keys back on the conversation, then <leader>/: one line, the search", () => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    const table: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "/"], "panel.search", "search")] };
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: table, newTabChord: "Ctrl+b c" });
    press(":", { shiftKey: true });
    act(() => root.focus());
    wait(400);
    press(" ");
    press("/");
    const lines = container.querySelectorAll<HTMLElement>(".search-bar");
    expect(lines).toHaveLength(1);
    expect(lines[0].querySelector("input")!.getAttribute("aria-label")).toBe("Search the conversation");
  });

  /* K02 fix round 1 (Codex, blocking): `:` (or `/`), then `prefix w` and Esc out of the chooser --
     or `prefix ,` and Esc out of a tab rename -- left the line drawn while that overlay's own exit
     gave the keys to the conversation under it, so going on with "the command", `l` then Enter,
     walked onto Approve and pressed it. Either overlay closes an open line as it takes the keys now,
     as a pane switch, a HINT and a tab switch already did: nothing is drawn that the keys are not in. */
  it.each([
    [":", "the chooser", () => dispatch({ kind: "chooser", open: [], records: [] }), ".chooser"],
    ["/", "the chooser", () => dispatch({ kind: "chooser", open: [], records: [] }), ".chooser"],
    [":", "a tab rename", () => dispatch({ kind: "begin_rename", tab: 1, current: null }), ".tab-rename"],
    ["/", "a tab rename", () => dispatch({ kind: "begin_rename", tab: 1, current: null }), ".tab-rename"],
  ])("K02: %s open, then %s takes the keys: the line closes, and leaving it draws no line", (lead, _name, open, selector) => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    wait(400);
    press(lead, lead === ":" ? { shiftKey: true } : {});
    expect(container.querySelector(".search-bar")).not.toBeNull();
    open();
    expect(container.querySelector(".search-bar")).toBeNull();
    const overlay = container.querySelector<HTMLElement>(selector)!;
    expect(document.activeElement).toBe(overlay);
    fireEvent.keyDown(overlay, { key: "Escape" });
    expect(container.querySelector(selector)).toBeNull();
    expect(document.activeElement).toBe(root);
    expect(container.querySelector(".search-bar")).toBeNull();
  });

  /* K02 fix round 2 (review): Tab in the `/` or `:` line was the browser's own focus move, onto the
     band's `.band-open` right after it, so the line stayed drawn while BROWSE had the keys: `:`, Tab,
     `l`, Enter walked onto Approve and pressed it, and a lone `a` answered. A line claims every Tab
     now, as vim's command line never leaves on one. And `:` or `/` again on a line already open (the
     keys moved off it by a click) only wiped it, so `l`, Enter went on to the card: it hands that
     line the keys back now. */
  /** Tab as a browser does it: the keydown, then -- only if its default survived -- focus onto the
   *  next focusable element in document order (jsdom does neither itself). Returns whether it did. */
  const tabOut = () => {
    const from = document.activeElement as HTMLElement;
    const survived = fireEvent.keyDown(from, { key: "Tab" });
    if (survived) {
      const stops = Array.from(
        document.querySelectorAll<HTMLElement>(
          "button:not([disabled]), input:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])",
        ),
      );
      const next = stops.find(
        (el) => !from.contains(el) && (from.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
      );
      if (next !== undefined) act(() => next.focus());
    }
    return survived;
  };
  /** Enter wherever focus is, with a focused button's own activation modelled (`pressOnButton`). */
  const enterAnywhere = () => {
    const target = document.activeElement as HTMLElement;
    if (fireEvent.keyDown(target, { key: "Enter" }) && target.tagName === "BUTTON") fireEvent.click(target);
  };
  const openLine = (lead: string) => press(lead, lead === ":" ? { shiftKey: true } : {});
  it.each([":", "/"])("K02: %s, Tab, then l and Enter at 400 ms a key: the line keeps the keys, and nothing is answered", (lead) => {
    const { container } = arrivedOnACard();
    wait(400);
    openLine(lead);
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    wait(400);
    const survived = tabOut();
    const afterTab = document.activeElement;
    wait(400);
    press("l");
    wait(400);
    enterAnywhere();
    wait(1000);
    expect(answered()).toEqual([]);
    expect(survived).toBe(false);
    expect(afterTab).toBe(input);
    expect(container.querySelector(".search-bar")).toBeNull();
  });
  it.each([":", "/"])("K02: %s, Tab, then a lone a: the a is the line's, and nothing is answered", (lead) => {
    const { container } = arrivedOnACard();
    wait(400);
    openLine(lead);
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    wait(400);
    tabOut();
    wait(400);
    press("a");
    wait(1000);
    expect(answered()).toEqual([]);
    expect(document.activeElement).toBe(input);
    expect(container.querySelector(".search-bar")).not.toBeNull();
  });
  it("K02: :, l, Tab, :, l, Enter at 400 ms a key answers nothing", () => {
    const { container } = arrivedOnACard();
    wait(400);
    openLine(":");
    wait(400);
    press("l");
    wait(400);
    tabOut();
    for (const key of [":", "l"]) {
      wait(400);
      press(key, key === ":" ? { shiftKey: true } : {});
    }
    wait(400);
    enterAnywhere();
    wait(1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".search-bar")).toBeNull();
  });
  it.each([":", "/"])("K02: %s's line claims every Tab: modified, and Shift+Tab's other WebKitGTK shape", (lead) => {
    const { container } = arrivedOnACard();
    openLine(lead);
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    for (const init of [
      { key: "Tab" },
      { key: "Tab", altKey: true },
      { key: "Tab", ctrlKey: true },
      { key: "Tab", metaKey: true },
      { key: "Unidentified", code: "Tab", shiftKey: true, altKey: true },
    ]) {
      expect(fireEvent.keyDown(input, init), JSON.stringify(init)).toBe(false);
    }
    expect(document.activeElement).toBe(input);
    expect(container.querySelector(".search-bar")).not.toBeNull();
  });
  it.each([":", "/"])("K02: %s open, the keys moved off it by a click, then that key again: its line takes the keys back", (lead) => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    wait(400);
    openLine(lead);
    act(() => root.focus()); // a click back on the conversation
    wait(400);
    openLine(lead);
    const lines = container.querySelectorAll<HTMLElement>(".search-bar");
    expect(lines).toHaveLength(1);
    const input = lines[0].querySelector("input")!;
    expect(document.activeElement).toBe(input);
    wait(400);
    press("l");
    wait(400);
    enterAnywhere();
    wait(1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".search-bar")).toBeNull();
  });
  it("K02: the / prompt open, the keys moved off it by a click, then <leader>/: the prompt takes the keys back", () => {
    const { container } = arrivedOnACard();
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    const table: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "/"], "panel.search", "search")] };
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: table, newTabChord: "Ctrl+b c" });
    openLine("/");
    act(() => root.focus()); // a click back on the conversation
    wait(400);
    press(" ");
    press("/");
    const lines = container.querySelectorAll<HTMLElement>(".search-bar");
    expect(lines).toHaveLength(1);
    expect(document.activeElement).toBe(lines[0].querySelector("input"));
  });
  it("K02: a Tab the line claimed is no landing: Approve focused by no key after it, then Enter, presses nothing", () => {
    const { container } = arrivedOnACard();
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    wait(400);
    openLine(":");
    wait(400);
    tabOut();
    act(() => approve.focus()); // a mouse pressed on Approve and dragged off it: focus, no click
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });

  /* K02 fix round 3 (review): fix round 2 read a Tab's default at the top of `onKeyDown`, ahead of the
     branches further down that swallow a plain Tab too -- K01's cancel after a waiting `g`/`z`/`[`/`]`,
     a key the leader's sequence does not bind. Such a Tab moves nothing, yet it armed the landing, so
     the next focus no key made (a mouse pressed on Approve and dragged off it) became its landing, and
     Enter pressed Approve. A Tab's default is judged once every handler has run now. */
  it.each([["g"], ["z"], ["["], ["]"]])(
    "K02: %s, then Tab (a cancelled pair: no focus move), then Approve focused by no key: Enter presses nothing",
    (prefix) => {
      const { container } = arrivedOnACard();
      const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
      const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
      wait(400);
      press(prefix);
      wait(400);
      expect(tabOut()).toBe(false);
      expect(document.activeElement).toBe(root);
      wait(400);
      act(() => approve.focus()); // a mouse pressed on Approve and dragged off it: focus, no click
      wait(400);
      expect(pressOnButton("Enter")).toBe(false);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(band(container)).toBe(ENTER_LANDING);
    },
  );
  it("K02: the leader, then Tab (a key its sequence does not bind), then Approve focused by no key: Enter presses nothing", () => {
    const { container } = arrivedOnACard();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    wait(400);
    press(" ");
    wait(400);
    expect(tabOut()).toBe(false);
    expect(document.activeElement).toBe(root);
    wait(400);
    act(() => approve.focus());
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_LANDING);
  });

  /* K02 fix round 3 (review): a card's button decides only the card `a`/`d` would from the cursor --
     the cursor's own, or the one gating the tool call it is on (`permissionTarget`, S4). A Tab walks
     on from one card's buttons into the next card's, and `l`/`h` then walk that card's (`currentStop`
     follows focus), while the row cursor stays where it was: Enter there answered a card the cursor
     was not on, one `a` in the same state would not have answered. */
  const ENTER_ELSEWHERE = "Enter answers the card under the cursor — j / k onto it first";
  /** Two cards waiting: `perm-1` (`rm build`, gating `toolu_1`) and, below it, `perm-2`
   *  (`rm -rf important`, gating `toolu_2`), the keys arrived on the oldest. Rows: the prompt 0,
   *  `toolu_1` 1, `perm-1` 2, `toolu_2` 3, `perm-2` 4. */
  function arrivedOnTheFirstOfTwoCards() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ capabilities: { ...initialState().capabilities, interrupt: true } }), 0);
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_2", name: "Bash", input: { command: "rm -rf important" } },
      {
        type: "permission_requested", permission_id: "perm-2", tool_use_id: "toolu_2", tool_name: "Bash",
        input: { command: "rm -rf important" },
      },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    const cards = Array.from(rendered.container.querySelectorAll<HTMLElement>(".permission-card"));
    expect(cards).toHaveLength(2);
    const controls = (card: HTMLElement) => ({
      reason: card.querySelector<HTMLInputElement>("input")!,
      approve: card.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!,
      deny: card.querySelector<HTMLButtonElement>('[data-nav-action="deny"]')!,
    });
    return { ...rendered, first: controls(cards[0]), second: controls(cards[1]) };
  }
  it("K02: l l onto Deny, Tab Tab onto the next card's Approve, then Enter: answers nothing, and says why", () => {
    const { container, first, second } = arrivedOnTheFirstOfTwoCards();
    press("l");
    press("l");
    expect(document.activeElement).toBe(first.deny);
    wait(400);
    tabOut();
    expect(document.activeElement).toBe(second.reason);
    wait(400);
    tabOut();
    expect(document.activeElement).toBe(second.approve);
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_ELSEWHERE);
  });
  it("K02: Tab onto the next card's Approve, then l onto its Deny, then Enter: answers nothing", () => {
    const { container, first, second } = arrivedOnTheFirstOfTwoCards();
    press("l");
    press("l");
    expect(document.activeElement).toBe(first.deny);
    wait(400);
    tabOut();
    wait(400);
    tabOut();
    wait(400);
    press("l");
    expect(document.activeElement).toBe(second.deny);
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(band(container)).toBe(ENTER_ELSEWHERE);
  });
  it("K02: j j onto the second card, then l and Enter: answers that card", () => {
    const { second } = arrivedOnTheFirstOfTwoCards();
    press("j");
    press("j");
    wait(400);
    press("l");
    expect(document.activeElement).toBe(second.approve);
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-2", decision: "allow" })]);
  });
  it("K02: the cursor on a tool call, Tab onto the Approve of the card gating it, then Enter: answers that card, as a does", () => {
    const { container, second } = arrivedOnTheFirstOfTwoCards();
    press("j");
    expect(container.querySelector(".row-current")!.classList.contains("row-tool")).toBe(true);
    act(() => second.reason.focus()); // a click into the card's reason box
    wait(400);
    tabOut();
    expect(document.activeElement).toBe(second.approve);
    wait(400);
    expect(pressOnButton("Enter")).toBe(false);
    wait(300);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-2", decision: "allow" })]);
  });

  /* v1 picks, Task 7 (ruling R7): `]p` / `[p` move the CURSOR to the next / previous card that waits for
     an answer in this tab, wrapping as nvim's `]d` does. They answer nothing, ever: no `permission_response`
     is posted, no button is pressed, and the S1 guard is untouched -- a lone `a`, 250 ms later, answers the
     card the cursor landed on (`permissionTarget`), and `a` typed hard on the heels of the jump answers
     nothing. Rows in `arrivedOnTheFirstOfTwoCards`: prompt 0, toolu_1 1, perm-1 2, toolu_2 3, perm-2 4. */
  describe("]p / [p: the cursor to the next / previous waiting card (v1 picks, Task 7, R7)", () => {
    const NO_WAITING = "no card waiting here";
    const COUNT_ANSWER = "a / d / D take no count — press it on its own";
    const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
    /** Which card the cursor is drawn on -- 1 or 2, by the Approve its row holds -- or 0 on neither. */
    const cardAt = (c: HTMLElement, cards: { approve: HTMLElement }[]) =>
      cards.findIndex((card) => c.querySelector(".row-current")!.contains(card.approve)) + 1;
    /** `]p` / `[p` as a person types them: the bracket, then the p. */
    const jump = (prefix: "]" | "[") => {
      press(prefix);
      press("p");
    };
    const top = () => {
      press("g");
      press("g");
    };
    /** The same conversation with no card in it at all. */
    function noCards() {
      const rendered = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "alpha" }, { seq: 2, text: "bravo" }] }), 3);
      act(() => root(rendered.container).focus());
      act(() => widen(rendered.container));
      return rendered;
    }

    it("gg, then ]p lands on card 1, ]p on card 2, ]p wraps to card 1 and [p goes back to card 2: nothing is answered", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      const cards = [first, second];
      top();
      expect(cardAt(container, cards)).toBe(0);
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      jump("]");
      expect(cardAt(container, cards)).toBe(1);
      jump("]");
      expect(cardAt(container, cards)).toBe(2);
      jump("]");
      expect(cardAt(container, cards)).toBe(1);
      jump("[");
      expect(cardAt(container, cards)).toBe(2);
      jump("[");
      expect(cardAt(container, cards)).toBe(1);
      wait(1000);
      // Not one answer, not one press of a button, and the keys stayed with the row.
      expect(answered()).toEqual([]);
      expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
      expect(document.activeElement).toBe(root(container));
    });

    it("[p from the top wraps to the last card, and a second [p goes on to the one before it", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      jump("[");
      expect(cardAt(container, [first, second])).toBe(2);
      jump("[");
      expect(cardAt(container, [first, second])).toBe(1);
      expect(answered()).toEqual([]);
    });

    it("a count repeats it round the ring: 2]p from the top is card 2, 3]p card 1, 2[p card 1, 3[p card 2", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      const cards = [first, second];
      for (const [digit, prefix, expected] of [["2", "]", 2], ["3", "]", 1], ["2", "[", 1], ["3", "[", 2]] as const) {
        top();
        press(digit);
        jump(prefix);
        expect(cardAt(container, cards), `${digit}${prefix}p`).toBe(expected);
      }
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("the largest count costs no more than one lap: 9999]p is card 1 and 9998]p card 2, as odd and even laps would be", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      const cards = [first, second];
      top();
      for (const d of "9999") press(d);
      jump("]");
      expect(cardAt(container, cards)).toBe(1);
      top();
      for (const d of "9998") press(d);
      jump("]");
      expect(cardAt(container, cards)).toBe(2);
    });

    it("the count goes to ]p and not to a: 2]p, then a lone a answers card 2 -- and only card 2 -- with no count flash", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      press("2");
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      expect(answered()).toEqual([]);
      wait(300);
      press("a");
      wait(300);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-2", decision: "allow" })]);
      expect(band(container)).not.toBe(COUNT_ANSWER);
    });

    it("after a jump, a lone d denies the card it landed on, whichever card the cursor left", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      jump("]");
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      wait(300);
      press("d");
      wait(300);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-2", decision: "deny" })]);
    });

    it("waits for its p as long as it takes: ], a 400 ms pause, then p still jumps", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      press("]");
      wait(400);
      press("p");
      expect(cardAt(container, [first, second])).toBe(1);
      expect(answered()).toEqual([]);
    });

    it("a d, or an a, after ] is swallowed with the prefix: ], d and ], a answer nothing and move nothing", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      for (const k of ["a", "d"]) {
        press("]");
        wait(400);
        press(k);
        wait(1000);
      }
      expect(answered()).toEqual([]);
      expect(cardAt(container, [first, second])).toBe(1);
    });

    /* S1 is unchanged: the jump is a key like any other, so an `a` right behind it -- `]pa`, typed as prose
       or a slip -- is refused with the typing flash, whichever card the cursor is on now. */
    it("an a typed hard on the heels of the jump answers nothing and says why", () => {
      const { container } = arrivedOnTheFirstOfTwoCards();
      top();
      jump("]");
      wait(80);
      press("a");
      wait(1000);
      expect(answered()).toEqual([]);
      expect(band(container)).toBe(TYPING);
    });

    it("from a card's own button the keys come back to the row: l onto Approve, then ]p, and a answers the card landed on", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      press("l");
      expect(document.activeElement).toBe(first.approve);
      wait(400);
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      expect(document.activeElement).toBe(root(container));
      wait(300);
      press("a");
      wait(300);
      // Never perm-1, whose Approve had the keys a moment ago.
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-2", decision: "allow" })]);
    });

    /* K02's Enter on a card's button needs a landing on that very button by the key right before it: the jump
       is neither, so an Enter after it -- on the row now, as the keys came back -- only folds or unfolds the
       card, and the button that had the keys before it is not pressed. */
    it("Enter right after a jump answers nothing: the keys are the row's, and the button they left is not pressed", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      press("l");
      expect(document.activeElement).toBe(first.approve);
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      expect(document.activeElement).toBe(root(container));
      for (const pause of [0, 300]) {
        wait(pause);
        expect(pressOnButton("Enter")).toBe(false); // claimed by the panel (a fold), never a native activation
      }
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("skips a card already answered from this panel, though it is still drawn", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      press("a");
      wait(1000);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1" })]);
      expect(first.approve.disabled).toBe(true);
      top();
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      // Card 2 is now the only one that waits: it is its own next and previous, and nothing flashes.
      jump("]");
      expect(cardAt(container, [first, second])).toBe(2);
      jump("[");
      expect(cardAt(container, [first, second])).toBe(2);
      expect(band(container)).not.toBe(NO_WAITING);
      expect(answered()).toHaveLength(1);
    });

    it("counts a card whose answer Rust refused as waiting again, with nothing to reset", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      press("a");
      wait(1000);
      const [firstAnswer] = answered();
      dispatch({ kind: "command_result", requestId: firstAnswer.request_id as string, ok: false, error: "could not save the rule: Permission denied" });
      top();
      jump("]");
      expect(cardAt(container, [first, second])).toBe(1);
    });

    it("with no card waiting -- every one answered -- flashes and leaves the cursor where it was", () => {
      const { container } = arrivedOnACard();
      press("a");
      wait(1000);
      expect(answered()).toHaveLength(1);
      top();
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      jump("]");
      expect(band(container)).toBe(NO_WAITING);
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      jump("[");
      expect(band(container)).toBe(NO_WAITING);
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      expect(answered()).toHaveLength(1);
    });

    it("with no card in the conversation at all, ]p and [p flash the same and leave the cursor", () => {
      const { container } = noCards();
      top();
      expect(container.querySelector(".row-current")!.textContent).toContain("alpha");
      jump("]");
      expect(band(container)).toBe(NO_WAITING);
      expect(container.querySelector(".row-current")!.textContent).toContain("alpha");
      wait(2100);
      expect(band(container)).toBeNull();
      jump("[");
      expect(band(container)).toBe(NO_WAITING);
      expect(container.querySelector(".row-current")!.textContent).toContain("alpha");
      expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
    });

    /* A card whose session ended is inert (`PermissionCard`'s `inert`; `a` on it does nothing): it does not
       wait for an answer, so the jump does not land on it. */
    it("does not count the inert cards of a session that ended: ]p flashes and the cursor stays", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      dispatch({ kind: "events", tab: 1, fromRevision: 6, throughRevision: 7, events: [{ type: "session_closed", reason: "provider exited" }] });
      expect(first.approve.disabled).toBe(true);
      expect(second.approve.disabled).toBe(true);
      top();
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      jump("]");
      expect(band(container)).toBe(NO_WAITING);
      expect(container.querySelector(".row-current")!.textContent).toContain("tidy up");
      jump("[");
      expect(cardAt(container, [first, second])).toBe(0);
      expect(answered()).toEqual([]);
    });

    /* A scroll the panel makes is announced on the list first, and `up` stops the list following a streaming
       reply (`./follow`): the way that counts is the cursor's, not the key's -- `]p` wrapping from the last
       card round to the first goes UP the list, `[p` wrapping the other way goes DOWN. */
    it("announces the way the cursor really travels: a wrapping ]p goes up, a wrapping [p goes down", () => {
      const { container } = arrivedOnTheFirstOfTwoCards();
      top();
      const list = container.querySelector(".message-list")!;
      const seen: unknown[] = [];
      list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
      jump("]"); // prompt -> card 1
      jump("]"); // card 1 -> card 2
      jump("]"); // card 2 -> card 1, round the ring
      jump("["); // card 1 -> card 2, round the ring
      jump("["); // card 2 -> card 1
      expect(seen).toEqual(["down", "down", "up", "down", "up"]);
    });

    it("a jump that stays -- the one card waiting is the one the cursor is on -- says nothing, and still takes the keys back", () => {
      const { container } = arrivedOnACard();
      const list = container.querySelector(".message-list")!;
      const seen: unknown[] = [];
      list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
      const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
      press("l");
      expect(document.activeElement).toBe(approve);
      wait(400);
      seen.length = 0; // `l` announces itself; only the jump is on trial
      jump("]");
      expect(container.querySelector(".row-current")!.contains(approve)).toBe(true);
      expect(seen).toEqual([]);
      expect(band(container)).not.toBe(NO_WAITING);
      // The keys are the row's again, so Enter cannot press the button they left, and a answers this card.
      expect(document.activeElement).toBe(root(container));
      wait(300);
      press("a");
      wait(300);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
    });

    it("the which-key box after ] lists p under the prompt pair, and a click on it jumps as typing it would", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      press("]");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      const box = container.querySelector(".which-key-box")!;
      expect(box.querySelector(".wk-title")!.textContent).toBe("]");
      expect(Array.from(box.querySelectorAll(".wk-entry")).map((e) => e.textContent)).toEqual(["]➜next prompt", "p➜next waiting card"]);
      fireEvent.click(Array.from(box.querySelectorAll(".wk-entry")).find((e) => e.textContent?.startsWith("p"))!);
      expect(cardAt(container, [first, second])).toBe(1);
      expect(container.querySelector(".which-key-box")).toBeNull();
      expect(answered()).toEqual([]);
    });

    it("the box after [ says the same for the way back", () => {
      const { container } = arrivedOnTheFirstOfTwoCards();
      press("[");
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      const box = container.querySelector(".which-key-box")!;
      expect(Array.from(box.querySelectorAll(".wk-entry")).map((e) => e.textContent)).toEqual(["[➜previous prompt", "p➜previous waiting card"]);
    });

    it("gives a composing p to the input method: ], then p mid-composition moves nothing and spends the prefix", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      press("]");
      press("p", { isComposing: true });
      press("p", { keyCode: 229 });
      expect(cardAt(container, [first, second])).toBe(0);
      // The prefix went with the composing key: a plain p afterwards is a plain p, not a jump.
      press("p");
      expect(cardAt(container, [first, second])).toBe(0);
      expect(answered()).toEqual([]);
    });

    it("a held p goes once: the first p jumps, its auto-repeats are a bare p and do nothing", () => {
      const { container, first, second } = arrivedOnTheFirstOfTwoCards();
      top();
      press("]");
      press("p");
      for (let i = 0; i < 4; i++) press("p", { repeat: true });
      expect(cardAt(container, [first, second])).toBe(1);
      expect(answered()).toEqual([]);
    });

    it("is BROWSE's: typed into the composer, ]p is text", () => {
      const { container } = arrivedOnTheFirstOfTwoCards();
      press("i");
      const box = container.querySelector<HTMLTextAreaElement>("textarea")!;
      expect(box).not.toBeNull();
      const notPrevented = [fireEvent.keyDown(box, { key: "]" }), fireEvent.keyDown(box, { key: "p" })];
      expect(notPrevented).toEqual([true, true]);
      expect(answered()).toEqual([]);
    });
  });
});

describe("App keyboard: every control is reachable with hjkl", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  /** Presses `key` wherever focus is, the way a real key arrives. */
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });

  /** A running turn with a tool call and the card gating it, on a provider that can interrupt, so
   *  the status line has a Stop button below the rows. */
  function withACardAndStop() {
    const rendered = started({ capabilities: { ...initialState().capabilities, interrupt: true } });
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    act(() => root(rendered.container).focus());
    return rendered;
  }

  // `a`/`d` answer after v1 S1's wait, and only when nothing came near them (spec 2026-09-27 §2.1).
  it("a on the tool call answers the card that gates it, without moving onto the card", () => {
    vi.useFakeTimers();
    try {
      const { container } = withACardAndStop();
      press("a");
      act(() => vi.advanceTimersByTime(250));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
      expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("d on the card itself denies it", () => {
    vi.useFakeTimers();
    try {
      withACardAndStop();
      press("j");
      act(() => vi.advanceTimersByTime(300));
      press("d");
      act(() => vi.advanceTimersByTime(250));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "deny" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("l walks the card's Approve, Deny and reason box; h and Esc come back to the row", () => {
    const { container } = withACardAndStop();
    press("j");
    press("l");
    expect(document.activeElement?.textContent).toBe("Approve");
    press("l");
    expect(document.activeElement?.textContent).toBe("Deny");
    press("h");
    press("h");
    expect(document.activeElement).toBe(root(container));
    press("l");
    press("l");
    press("l");
    expect((document.activeElement as HTMLElement).tagName).toBe("INPUT");
    // In the reason box letters are text, so only Esc leaves it.
    expect(press("h")).toBe(true);
    press("Escape");
    expect(document.activeElement).toBe(root(container));
  });

  it("j past the last row lands on Stop, draws the row cursor hollow, and k comes back", () => {
    const { container } = withACardAndStop();
    press("j");
    press("j");
    expect(document.activeElement?.textContent).toBe("ctrl+c interrupt");
    expect(container.querySelector(".message-list")!.getAttribute("data-focused")).toBe("false");
    press("k");
    expect(document.activeElement).toBe(root(container));
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });

  it("does not answer a card from Stop, where the row cursor is not what the keys act on", () => {
    vi.useFakeTimers();
    try {
      withACardAndStop();
      press("j");
      press("j");
      // Long enough that v1 S1's guard is not what refuses it.
      act(() => vi.advanceTimersByTime(300));
      press("a");
      act(() => vi.advanceTimersByTime(1000));
      expect(lastOfType("permission_response")).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("hands a key that lands on <body> back to the panel", () => {
    const { container } = withACardAndStop();
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    act(() => {
      fireEvent.keyDown(document.body, { key: "j" });
    });
    expect(document.activeElement).toBe(root(container));
    // And the key itself was not lost: the cursor moved onto the card.
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });

  // "walks the start screen's buttons with j and k" was removed here (session tabs Task 9): the
  // two boxed Auto/Bypass mode buttons this test walked are gone with the mode-selector start
  // screen, and the empty tab (F3) that replaced it owns its own `j`/`k` handling entirely --
  // separate from this describe block's `root`/`press`, which only reach the CONVERSATION's
  // `onKeyDown` once a session is live. The empty tab's own resume-row walking is unit-tested
  // directly in `components/EmptyTab.test.tsx`.
});

/* The panel's half of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
   `shell` owns the session and every key typed during it; the panel answers five envelopes. jsdom lays
   nothing out, so `layOut` gives every stop, control and code block a 10px box inside a 1000px list,
   one below the other, and the list and root the whole height. Nothing here shows what WebKit draws. */
describe("App global HINT: the panel's half", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });
  const labels = (c: HTMLElement) => Array.from(c.querySelectorAll<HTMLElement>(".hint-label"));
  const box = (el: Element, top: number, height = 10) => {
    (el as HTMLElement).getBoundingClientRect = () =>
      ({ top, bottom: top + height, left: 0, right: 100, width: 100, height, x: 0, y: top }) as DOMRect;
  };
  function layOut(container: HTMLElement) {
    box(container.querySelector(".agent-ui-root")!, 0, 1000);
    const list = container.querySelector(".message-list");
    if (list !== null) box(list, 0, 1000);
    let y = 0;
    for (const el of container.querySelectorAll("[data-nav-stop], button, input, pre.code-block, .row-sign")) {
      box(el, y);
      y += 10;
    }
  }

  /** A prompt, a reply carrying one fenced code block, and a tool call gated by a pending card (an
   *  Approve, a Deny and a reason box), on a provider that can interrupt so Stop shows too. */
  function conversation(replyTail = "then look.") {
    const rendered = started({ capabilities: { ...initialState().capabilities, interrupt: true } });
    events(
      { type: "user_prompt_submitted", text: "list it" },
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "Run this:\n\n```\nls -la\n```\n\n" + replyTail },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    layOut(rendered.container);
    act(() => root(rendered.container).focus());
    return rendered;
  }

  /** Runs `hint_collect` for `sessionId` and returns the count the panel reported. */
  function collect(sessionId: number): number {
    dispatch({ kind: "hint_collect", sessionId });
    const reply = lastOfType("hint_targets");
    expect(reply).toMatchObject({ session_id: sessionId });
    expect(typeof reply!.request_id).toBe("string");
    return reply!.count as number;
  }
  const LETTERS = ["a", "s", "d", "j", "k", "l", "g", "h", "w", "e", "r", "u", "i", "o"];
  function show(sessionId: number, count: number) {
    dispatch({ kind: "hint_show", sessionId, labels: LETTERS.slice(0, count) });
  }
  /** Where each target of `conversation()` sits in the frozen list, i.e. the index `shell` sends:
   *  rows in document order, each followed by its code blocks and then its controls, the card's in
   *  `data-nav-order` (Approve, Deny, reason), then the activity line's Stop. **Correction (C1c, spec
   *  §3.4):** the band used to be one more target past `stop` (its `.band-open`, `data-nav-stop=
   *  "status-band"`'s own only child that matched `controlsOf`'s selector) -- dropping that attribute
   *  so the band stops being a `j`/`k` stop also drops it out of `hintTargets` (`nav.ts` builds both
   *  lists from the same `stopsIn`), so HINT no longer reaches it either; its details are still
   *  reachable on `<leader>i`, `prefix i` and a click, none of which go through this list. */
  const AT = { prompt: 0, reply: 1, code: 2, tool: 3, card: 4, approve: 5, deny: 6, reason: 7, stop: 8 };

  it("f in BROWSE asks shell for a HINT TYPING_GUARD_MS later, and f in INPUT is just a letter", () => {
    // v1 hardening R2-1: `f` is always the first key of whatever typed it, so it defers the same
    // `TYPING_GUARD_MS` `a`/`d` do rather than asking `shell` at once -- a lone `f` still asks, just
    // that much later.
    vi.useFakeTimers();
    try {
      const { container } = conversation();
      press("f");
      expect(lastOfType("hint_request")).toBeUndefined();
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(lastOfType("hint_request")).toMatchObject({ type: "hint_request" });
      expect(typeof lastOfType("hint_request")!.request_id).toBe("string");
      // shell answers and the HINT ends; only then are the panel's keys its own again.
      collect(1);
      dispatch({ kind: "hint_end", sessionId: 1 });
      posted = [];
      fireEvent.keyDown(root(container), { key: "i" });
      fireEvent.keyDown(container.querySelector("textarea")!, { key: "f" });
      expect(lastOfType("hint_request")).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  /** v1 hardening R2-1's own reproduction: "fix the dashboard layout⏎" typed at typing speed used to
   *  start a HINT on its own `f` (`mayActAfterMotion` cannot catch this: `f` is always the first
   *  key). Here the `i` that follows within `TYPING_GUARD_MS` cancels the deferred `f` before it
   *  ever posts, and the band says so in `f`'s own words, not the `a`/`d` text it used to show with
   *  no card anywhere (the whole-branch review). */
  it('"fix this" at 80 ms a key asks shell for no HINT, and the band names f, not a / d', () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = conversation();
      act(() => widen(container));
      for (const key of ["f", "i", "x", " ", "t", "h", "i", "s"]) {
        press(key);
        act(() => vi.advanceTimersByTime(80));
      }
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(lastOfType("hint_request")).toBeUndefined();
      expect(container.querySelector(".band-message")?.textContent).toBe(hintTypingFlash());
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** `f` itself arriving soon after another key (rather than being cancelled by one that follows)
   *  refuses at once and says so with its own message: `j` (an ordinary move) then `f` within the
   *  window. */
  it("j then f at 80 ms a key: f refuses at once, and the band names it", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = conversation();
      act(() => widen(container));
      press("j");
      act(() => vi.advanceTimersByTime(80));
      press("f");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(lastOfType("hint_request")).toBeUndefined();
      expect(container.querySelector(".band-message")?.textContent).toBe(hintTypingFlash());
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /* Whole-branch review: between `f` posting `hint_request` and shell attaching its window key
     controller (which happens only once it handles that script message), keys still reached this
     panel's own table. */
  describe("between f and shell's HINT taking the keys, no key acts in the panel", () => {
    /** The conversation, with the row cursor on the pending permission card. */
    function onTheCard() {
      const rendered = conversation();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: AT.card });
      expect(rendered.container.querySelector(".row-current")).toBe(rendered.container.querySelector(".row-permission"));
      posted = [];
      return rendered;
    }

    it("a label letter typed in the gap does not answer the card under the cursor", () => {
      onTheCard();
      // Control: without a HINT, `a` on this row approves.
      press("f");
      press("a");
      press("d");
      expect(lastOfType("permission_response")).toBeUndefined();
    });

    it("i does not open the composer, j does not move, a second f asks nothing", () => {
      vi.useFakeTimers();
      try {
        const { container } = onTheCard();
        // The first `f` stands alone and is left to run for real (v1 hardening R2-1's own defer,
        // TYPING_GUARD_MS later) before the round-trip gap this test is actually about begins.
        press("f");
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
        press("i");
        press("j");
        press("f");
        expect(container.querySelector("textarea")).toBeNull();
        expect(container.querySelector(".row-current")).toBe(container.querySelector(".row-permission"));
        expect(posted.filter((m) => m.type === "hint_request")).toHaveLength(1);
      } finally {
        vi.useRealTimers();
      }
    });

    it("once the HINT it asked for has ended, the keys are the panel's again", () => {
      vi.useFakeTimers();
      try {
        const { container } = onTheCard();
        press("f");
        collect(2);
        dispatch({ kind: "hint_end", sessionId: 2 });
        // The `f` was a key: a lone `a` waits for nothing near it (v1 S1, spec 2026-09-27 §2.1).
        act(() => vi.advanceTimersByTime(300));
        press("a");
        act(() => vi.advanceTimersByTime(250));
        expect(lastOfType("permission_response")).toBeDefined();
        expect(container.querySelector(".row-permission")).not.toBeNull();
      } finally {
        vi.useRealTimers();
      }
    });

    it("a shell that never answers costs a second of dead keys, not a stuck panel", () => {
      vi.useFakeTimers();
      try {
        const { container } = onTheCard();
        press("f");
        // The lone `f` runs for real (v1 hardening R2-1's own defer) before the dead-key window
        // this test is actually about starts counting.
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
        act(() => vi.advanceTimersByTime(HINT_PENDING_TIMEOUT_MS - 1));
        press("i");
        expect(container.querySelector("textarea")).toBeNull();
        act(() => vi.advanceTimersByTime(1));
        press("i");
        expect(container.querySelector("textarea")).not.toBeNull();
      } finally {
        vi.useRealTimers();
      }
    });

    it("a held f's auto-repeat asks for nothing", () => {
      onTheCard();
      fireEvent.keyDown(document.activeElement!, { key: "f", repeat: true });
      expect(lastOfType("hint_request")).toBeUndefined();
    });
  });

  /** A prompt and nothing running: the composer can take a message. */
  function idle() {
    const rendered = started();
    events({ type: "user_prompt_submitted", text: "hello" });
    layOut(rendered.container);
    box(rendered.container.querySelector(".composer")!, 900);
    act(() => root(rendered.container).focus());
    return rendered;
  }

  it("labels the composer and landing there enters INPUT with the caret in the box", () => {
    const { container } = idle();
    // The prompt row, then the composer (C1c: the band is no longer a target -- see AT's own doc
    // comment).
    expect(collect(1)).toBe(2);
    show(1, 2);
    expect(labels(container)[1].classList.contains("hint-composer")).toBe(true);
    dispatch({ kind: "hint_land", sessionId: 1, index: 1 });
    const textarea = container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    expect(document.activeElement).toBe(textarea);
    expect(lastOfType("send_message")).toBeUndefined();
  });

  // Fix round 1 (reviewer finding): `landOnHint`'s "composer" branch carries the same caret rule
  // `i`/`o` do -- "kept" -- rather than whatever `composerCaret` was last left at by an earlier `A`
  // press (that press bumps `composerFocusRequest` directly, never `inputRequest`, so nothing about
  // it should leak into a later HINT landing). Reproduced without this fix: the caret below landed
  // at the end of the draft, not at 3.
  it("landing on the composer via HINT keeps the caret where it was, not wherever an earlier A left composerCaret", () => {
    const { container } = idle();
    fireEvent.keyDown(root(container), { key: "A", shiftKey: true }); // composerCaret -> "end", once.
    const box1 = container.querySelector("textarea")!;
    fireEvent.change(box1, { target: { value: "hello world" } });
    box1.setSelectionRange(3, 3);
    fireEvent.keyUp(box1, { key: "ArrowLeft" });
    fireEvent.keyDown(box1, { key: "Escape" });
    // Land back on the composer via HINT, never via i/o/A again.
    expect(collect(1)).toBe(2);
    show(1, 2);
    dispatch({ kind: "hint_land", sessionId: 1, index: 1 });
    const box2 = container.querySelector("textarea")!;
    expect(box2.selectionStart).toBe(3);
    expect(box2.selectionEnd).toBe(3);
  });

  /** v1 trial fix round 2 (Codex): a bare `/model`'s reply that arrived while a HINT was up opened
   *  `SlashPicker` under the labels -- `hint_collect` closed the chooser and `?` but left the pending
   *  picker armed, and `slashReply`'s overlay check did not count a HINT. Landing on the composer then
   *  left the keys on the picker, while GTK went on taking the label keys. A HINT is a route away from
   *  the picker the way `prefix w` is (finding 4): it closes an open one and drops a pending one. */
  describe("and the /model picker (v1 trial fix round 2)", () => {
    const MODEL_REPLY =
      "Current model: `Haiku 4.5` (effort: high)\n" +
      "Usage: /model <name>. Available: sonnet, opus, haiku, or a full model ID.";
    function reply() {
      events(
        { type: "turn_started", turn_id: "t1" },
        { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
      );
    }
    /** `idle()`, then a bare `/model` sent from INPUT, left in BROWSE with the keys on the root. */
    function sentBareModel() {
      const rendered = idle();
      fireEvent.keyDown(root(rendered.container), { key: "i" });
      const textarea = rendered.container.querySelector("textarea")!;
      fireEvent.change(textarea, { target: { value: "/model" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(lastOfType("send_message")!.text).toBe("/model");
      fireEvent.keyDown(rendered.container.querySelector("textarea")!, { key: "Escape" });
      layOut(rendered.container);
      box(rendered.container.querySelector(".composer")!, 900);
      return rendered;
    }

    it("a reply that lands while the labels are up opens no picker, and landing on the composer types there", () => {
      const { container } = sentBareModel();
      expect(collect(1)).toBe(2);
      show(1, 2);
      reply();
      expect(container.querySelector(".slash-picker")).toBeNull();
      expect(labels(container)).toHaveLength(2);
      dispatch({ kind: "hint_land", sessionId: 1, index: 1 });
      expect(container.querySelector(".slash-picker")).toBeNull();
      expect(document.activeElement).toBe(container.querySelector("textarea"));
    });

    it("a HINT drops the pending picker: the reply opens nothing after the HINT ends either", () => {
      const { container } = sentBareModel();
      collect(1);
      dispatch({ kind: "hint_end", sessionId: 1 });
      reply();
      expect(container.querySelector(".slash-picker")).toBeNull();
    });

    it("a HINT closes an open picker, as it closes the chooser", () => {
      const { container } = sentBareModel();
      reply();
      expect(container.querySelector(".slash-picker")).not.toBeNull();
      collect(1);
      expect(container.querySelector(".slash-picker")).toBeNull();
    });
  });

  it("offers no composer label only once the session has ended (C1: a running turn no longer disables it)", () => {
    // A running turn no longer disables the composer (C1: it queues a follow-up instead), so it is
    // a HINT target through the turn too -- one more than `AT`'s own count.
    const running = conversation();
    box(running.container.querySelector(".composer")!, 900);
    expect(collect(1)).toBe(Object.keys(AT).length + 1);
    cleanup();
    const { container } = idle();
    expect(collect(2)).toBe(2);
    events({ type: "session_closed", reason: "done" });
    layOut(container);
    box(container.querySelector(".composer")!, 900);
    show(3, collect(3));
    expect(container.querySelector(".hint-composer")).toBeNull();
    expect(container.querySelector("[data-hint-composer]")).toBeNull();
  });

  it("drops the label of a target that has scrolled out of the list, and keeps its letter's place", () => {
    const { container } = conversation();
    const count = collect(1);
    show(1, count);
    expect(labels(container)).toHaveLength(count);
    // The list follows a streamed reply to its end: the prompt row moves above the list's top edge.
    const prompt = container.querySelector('[data-nav-stop="row"]')!;
    box(prompt, -50);
    box(prompt.querySelector(".row-sign")!, -50);
    // Any commit re-measures; a prefix is one.
    dispatch({ kind: "hint_prefix", sessionId: 1, typed: "d" });
    const left = labels(container);
    expect(left).toHaveLength(count - 1);
    expect(left.map((l) => l.textContent)).toEqual(LETTERS.slice(1, count));
  });

  it("f on the empty tab asks shell for a HINT too, once the composer stops taking the keys", () => {
    // C1: a `starting` tab's composer is live now (it can queue behind the connect), so `f` there is
    // a character like any other, same as the ordinary NotStarted case -- only once the box is out
    // of INPUT does `f` mean HINT (`EmptyTab`'s own `onKeyDown`), the same as the ended/lost
    // banners' `r`. Deviation from the brief's own parenthetical ("pressing Escape first"): `EmptyTab`
    // has no `Escape` handling of its own (only `keymap.ts`'s conversation-root table does), so this
    // uses the same blur `Composer`'s own BROWSE/INPUT split already relies on everywhere else.
    // Fix round 1 (R2-1's own copy for `EmptyTab`): `f` there now defers `TYPING_GUARD_MS` too, the
    // same as the live conversation's own `f` (`typingGuard.ts`'s own doc comment) -- a lone `f`
    // still asks, just that much later. The composer's own `f` still reaches `typingGuard.onKey`
    // (called unconditionally, ahead of the mode check, this file's own doc comment) even though
    // the mode gate swallows it, so the SECOND `f` below needs a real gap behind it too, or the
    // guard reads it as arriving too soon after the first -- the thing it exists to catch.
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
      const textarea = container.querySelector("textarea")!;
      fireEvent.keyDown(textarea, { key: "f" });
      expect(lastOfType("hint_request")).toBeUndefined();
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      fireEvent.blur(textarea);
      fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "f" });
      expect(lastOfType("hint_request")).toBeUndefined();
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS));
      expect(lastOfType("hint_request")).toBeDefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("hint_collect reports how many targets are on screen, and leaves off-screen ones out", () => {
    const { container } = conversation();
    // 4 rows (prompt, reply, tool call, card) + the reply's code block + Approve + Deny + the
    // reason box + Stop (C1c: the band no longer counts -- see AT's own doc comment).
    const count = collect(1);
    expect(count).toBe(Object.keys(AT).length);
    // Push the card row (and everything in it) below the list's viewport: it is no longer counted.
    const card = container.querySelector(".row-permission")!;
    box(card, 2000);
    for (const el of card.querySelectorAll("button, input")) box(el, 2000);
    expect(collect(2)).toBe(5);
  });

  it("hint_show draws one label per frozen target, a row's over its sign cell", () => {
    const { container } = conversation();
    const count = collect(1);
    show(1, count);
    expect(labels(container)).toHaveLength(count);
    expect(labels(container).map((l) => l.textContent)).toEqual(LETTERS.slice(0, count));
    const kinds = labels(container).map((l) => l.className.split(" ")[1]);
    expect(kinds).toContain("hint-row");
    expect(kinds).toContain("hint-code");
    expect(kinds).toContain("hint-control");
    // A row's label sits exactly on its sign cell, not at the row's own corner.
    const firstSign = container.querySelector('[data-nav-stop="row"] .row-sign')!;
    const firstRow = container.querySelector('[data-nav-stop="row"]')!;
    expect(firstSign.getBoundingClientRect().top).not.toBe(firstRow.getBoundingClientRect().top);
    expect(labels(container)[AT.prompt].style.top).toBe(`${firstSign.getBoundingClientRect().top}px`);
  });

  it("hint_prefix dims the typed part and greys every label it rules out", () => {
    const { container } = conversation();
    collect(1);
    dispatch({ kind: "hint_show", sessionId: 1, labels: ["aa", "as", "sa", "ss", "da", "ds", "ja", "js", "ka"] });
    dispatch({ kind: "hint_prefix", sessionId: 1, typed: "a" });
    const [first, second, third] = labels(container);
    expect(first.classList.contains("hint-off")).toBe(false);
    expect(first.querySelector(".hint-typed")!.textContent).toBe("a");
    expect(second.classList.contains("hint-off")).toBe(false);
    expect(third.classList.contains("hint-off")).toBe(true);
    expect(labels(container).filter((l) => l.classList.contains("hint-off"))).toHaveLength(7);
  });

  it("hint_land on a row moves the cursor there and back into BROWSE, and clears the labels", () => {
    const { container } = conversation();
    const count = collect(1);
    show(1, count);
    const toolRow = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'))[2];
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.tool });
    expect(container.querySelector(".row-current")).toBe(toolRow);
    expect(document.activeElement).toBe(root(container));
    expect(labels(container)).toHaveLength(0);
  });

  it("hint_land on a button focuses it and never presses it", () => {
    const { container } = conversation();
    show(1, collect(1));
    const approve = buttonLabelled(container, "Approve")!;
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.approve });
    expect(document.activeElement).toBe(approve);
    expect(lastOfType("permission_response")).toBeUndefined();
    // The row cursor followed it onto the card, so h hands the keys back to THAT row.
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });

  /* K02 (ruling R3): a HINT landing is a landing -- the Enter right after it presses the button,
     `TYPING_GUARD_MS` later, as `l` then Enter does. A key in between makes it none. */
  it("K02: a HINT landing on Approve, then Enter, approves after S1's wait", () => {
    vi.useFakeTimers();
    try {
      conversation();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: AT.approve });
      act(() => vi.advanceTimersByTime(300));
      expect(fireEvent.keyDown(document.activeElement!, { key: "Enter" })).toBe(false);
      expect(lastOfType("permission_response")).toBeUndefined();
      act(() => vi.advanceTimersByTime(300));
      expect(lastOfType("permission_response")).toMatchObject({ decision: "allow" });
    } finally {
      vi.useRealTimers();
    }
  });
  it("K02: a HINT landing on Approve, then x, then Enter, presses nothing", () => {
    vi.useFakeTimers();
    try {
      conversation();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: AT.approve });
      act(() => vi.advanceTimersByTime(300));
      press("x");
      act(() => vi.advanceTimersByTime(300));
      expect(fireEvent.keyDown(document.activeElement!, { key: "Enter" })).toBe(false);
      act(() => vi.advanceTimersByTime(1000));
      expect(lastOfType("permission_response")).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("hint_land on a button from INPUT leaves the button focused, not the root", () => {
    const { container } = conversation();
    show(1, collect(1));
    fireEvent.keyDown(root(container), { key: "i" });
    act(() => container.querySelector("textarea")!.focus());
    const stop = buttonLabelled(container, "ctrl+c interrupt")!;
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.stop });
    expect(document.activeElement).toBe(stop);
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("hint_land on the reason box gives it the keys", () => {
    conversation();
    show(1, collect(1));
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.reason });
    expect((document.activeElement as HTMLElement).tagName).toBe("INPUT");
    expect(document.activeElement!.closest(".row-permission")).not.toBeNull();
  });

  /* v1 picks, Task 8 (R6): a web link in a reply is a HINT target, after its row's code blocks and before
     its controls. Landing FOCUSES the anchor (never clicks it) with the reply's row current in BROWSE, and
     Enter on it is the browser's own activation -- the native `LinkClicked` path -- not a key this panel claims. */
  describe("web links (v1 picks, Task 8, R6)", () => {
    const REPLY = "then look at https://example.com/a.";
    /** `conversation()` with the link in the reply, its anchor on screen. Returns the anchor and where it sits in the list. */
    function withLink() {
      const rendered = conversation(REPLY);
      const anchor = rendered.container.querySelector<HTMLAnchorElement>('.row-assistant a[href]')!;
      box(anchor, 500);
      return { ...rendered, anchor, at: 3 };
    }

    it("counts one more target for a link on screen, and puts it right after the reply's code block", () => {
      const { container, anchor, at } = withLink();
      const count = collect(1);
      expect(count).toBe(Object.keys(AT).length + 1);
      show(1, count);
      const kinds = labels(container).map((l) => l.className.split(" ")[1]);
      expect(kinds[AT.code]).toBe("hint-code");
      expect(kinds[at]).toBe("hint-link");
      expect(kinds[at + 1]).toBe("hint-row"); // the tool call's row, which used to follow the code block
      expect(anchor.isConnected).toBe(true);
    });

    it("counts no link that is not on screen, or not a web link", () => {
      const { container, anchor } = withLink();
      box(anchor, 5000); // scrolled off the list
      expect(collect(1)).toBe(Object.keys(AT).length);
      box(anchor, 500);
      anchor.setAttribute("href", "docs/a.md");
      expect(collect(2)).toBe(Object.keys(AT).length);
      expect(container.querySelector(".hint-layer")).toBeNull();
    });

    it("hint_land on a link focuses the anchor and makes its row current in BROWSE, pressing nothing", () => {
      const { container, anchor, at } = withLink();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: at });
      expect(document.activeElement).toBe(anchor);
      expect(container.querySelector(".row-current")!.classList.contains("row-assistant")).toBe(true);
      expect(container.querySelector(".row-current")!.contains(anchor)).toBe(true);
      expect(labels(container)).toHaveLength(0);
      expect(posted.filter((m) => m.type === "open_url" || m.type === "permission_response")).toEqual([]);
    });

    it("Enter on the landed link is left to the browser: not prevented, and no open_url from the panel", () => {
      const { anchor, at } = withLink();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: at });
      expect(fireEvent.keyDown(document.activeElement!, { key: "Enter" })).toBe(true);
      expect(posted.filter((m) => m.type === "open_url")).toEqual([]);
      expect(anchor.isConnected).toBe(true);
    });

    it("Space on the landed link is the browser's too: not claimed, not the leader", () => {
      const { at } = withLink();
      show(1, collect(1));
      dispatch({ kind: "hint_land", sessionId: 1, index: at });
      expect(fireEvent.keyDown(document.activeElement!, { key: " " })).toBe(true);
      expect(posted.filter((m) => m.type === "open_url")).toEqual([]);
    });

    it("a landed link is not a card button: Enter on it answers nothing, however long after", () => {
      vi.useFakeTimers();
      try {
        const { at } = withLink();
        show(1, collect(1));
        dispatch({ kind: "hint_land", sessionId: 1, index: at });
        act(() => vi.advanceTimersByTime(300));
        fireEvent.keyDown(document.activeElement!, { key: "Enter" });
        act(() => vi.advanceTimersByTime(1000));
        expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
      } finally {
        vi.useRealTimers();
      }
    });

    it("draws the label of a link on its first line: a link wrapped onto two lines is labelled where it starts", () => {
      const { container, anchor, at } = withLink();
      const first = { top: 610, left: 40, bottom: 630, right: 300, width: 260, height: 20, x: 40, y: 610 } as DOMRect;
      const second = { top: 630, left: 0, bottom: 650, right: 90, width: 90, height: 20, x: 0, y: 630 } as DOMRect;
      anchor.getClientRects = () => [first, second] as unknown as DOMRectList;
      // The bounding box of a wrapped link starts at the row's left edge, on its first line.
      anchor.getBoundingClientRect = () =>
        ({ top: 610, bottom: 650, left: 0, right: 300, width: 300, height: 40, x: 0, y: 610 }) as DOMRect;
      show(1, collect(1));
      const origin = root(container).getBoundingClientRect();
      const label = labels(container)[at];
      expect(label.classList.contains("hint-link")).toBe(true);
      expect(label.style.top).toBe(`${first.top - origin.top - 6}px`);
      expect(label.style.left).toBe(`${first.left - origin.left - 6}px`);
    });

    it("labels a wrapped link on a line that is on screen: the first line scrolled above the list is not where it goes", () => {
      // Fix round 1 (Codex): `hintVisible` takes the link's whole box, so a link whose first line has left
      // the list but whose last line is in it is a target -- its label must sit on the visible line.
      const { container, anchor, at } = withLink();
      const first = { top: -30, left: 40, bottom: -10, right: 300, width: 260, height: 20, x: 40, y: -30 } as DOMRect;
      const second = { top: 0, left: 0, bottom: 20, right: 90, width: 90, height: 20, x: 0, y: 0 } as DOMRect;
      anchor.getClientRects = () => [first, second] as unknown as DOMRectList;
      anchor.getBoundingClientRect = () =>
        ({ top: -30, bottom: 20, left: 0, right: 300, width: 300, height: 50, x: 0, y: -30 }) as DOMRect;
      const count = collect(1);
      expect(count).toBe(Object.keys(AT).length + 1); // still a target: part of it is in view
      show(1, count);
      const origin = root(container).getBoundingClientRect();
      const label = labels(container)[at];
      expect(label.classList.contains("hint-link")).toBe(true);
      // K07 and its fix round: the chosen line's label is clamped into the list (x 0, y 0 here), so it no
      // longer hangs 6px outside it; the first line's would be at x 34.
      expect(label.style.top).toBe(`${Math.max(second.top - 6, 0) - origin.top}px`);
      expect(label.style.left).toBe(`${Math.max(second.left - 6, 0) - origin.left}px`);
    });

    it("falls back to the bounding box where the browser reports no line boxes", () => {
      const { container, anchor, at } = withLink();
      show(1, collect(1));
      const origin = root(container).getBoundingClientRect();
      const r = anchor.getBoundingClientRect();
      expect(anchor.getClientRects()).toHaveLength(0); // jsdom measures none
      expect(labels(container)[at].style.top).toBe(`${r.top - origin.top - 6}px`);
      // K07 fix round: clamped into the link's clip box (the list, here at x 0), never left of it.
      expect(labels(container)[at].style.left).toBe(`${Math.max(r.left - 6, 0) - origin.left}px`);
    });
  });

  it("hint_land on a code block makes the next y copy only that block's code, once", () => {
    const { container } = conversation();
    const clipboard = stubClipboard();
    show(1, collect(1));
    expect(labels(container)[AT.code].classList.contains("hint-code")).toBe(true);
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.code });
    expect(container.querySelector(".row-current")!.classList.contains("row-assistant")).toBe(true);
    press("y");
    expect(clipboard.writeText).toHaveBeenLastCalledWith("ls -la");
    // Only the next y: the one after copies the whole message again.
    press("y");
    expect(clipboard.writeText).toHaveBeenLastCalledWith(expect.stringContaining("then look."));
  });

  it("any other key after landing on a code block forgets it", () => {
    conversation();
    const clipboard = stubClipboard();
    show(1, collect(1));
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.code });
    press("j");
    press("k");
    press("y");
    expect(clipboard.writeText).toHaveBeenLastCalledWith(expect.stringContaining("then look."));
  });

  it("hint_end leaves no label behind and moves nothing", () => {
    const { container } = conversation();
    show(1, collect(1));
    const before = container.querySelector(".row-current");
    dispatch({ kind: "hint_end", sessionId: 1 });
    expect(labels(container)).toHaveLength(0);
    expect(container.querySelector(".hint-layer")).toBeNull();
    expect(container.querySelector(".row-current")).toBe(before);
    // The session is over: a late land for it does nothing.
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.tool });
    expect(container.querySelector(".row-current")).toBe(before);
  });

  it("ignores every envelope that names a session other than the current one", () => {
    const { container } = conversation();
    const count = collect(2);
    const before = container.querySelector(".row-current");
    show(1, count);
    expect(labels(container)).toHaveLength(0);
    show(2, count);
    dispatch({ kind: "hint_prefix", sessionId: 1, typed: "a" });
    expect(labels(container).some((l) => l.classList.contains("hint-off"))).toBe(false);
    dispatch({ kind: "hint_land", sessionId: 1, index: AT.tool });
    dispatch({ kind: "hint_end", sessionId: 1 });
    expect(labels(container)).toHaveLength(count);
    expect(container.querySelector(".row-current")).toBe(before);
  });

  it("hint_land on a row finds the row where it is NOW, not where it was when frozen", () => {
    // Two tool calls running; B's row is frozen at hint_collect. A's permission card then arrives
    // and is anchored straight after A (timeline.ts), pushing B one row down during HINT.
    const rendered = started();
    events(
      { type: "user_prompt_submitted", text: "go" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_A", name: "Bash", input: { cmd: "a" } },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_B", name: "Bash", input: { cmd: "b" } },
    );
    const { container } = rendered;
    layOut(container);
    act(() => root(container).focus());
    const rows = () => Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    const b = rows()[2];
    expect(b.textContent).toContain("b");
    show(1, collect(1));
    events({ type: "permission_requested", permission_id: "perm-A", tool_use_id: "toolu_A", tool_name: "Bash", input: {} });
    expect(rows().indexOf(b)).toBe(3); // B moved, and is still the same element
    dispatch({ kind: "hint_land", sessionId: 1, index: 2 }); // B's frozen index: prompt, A, B
    expect(container.querySelector(".row-current")).toBe(b);
  });

  it("draws no label for a frozen target that has left the page since", () => {
    // The card is answered while the labels are up: its row, Approve, Deny and reason box are gone.
    // Their labels must go too, not be re-measured as all-zero rects at the panel's corner.
    const { container } = conversation();
    const count = collect(1);
    show(1, count);
    expect(labels(container)).toHaveLength(count);
    events({ type: "permission_resolved", permission_id: "perm-1", outcome: "cancelled_by_session_close" });
    expect(container.querySelector(".row-permission")).toBeNull();
    const left = labels(container);
    expect(left).toHaveLength(count - 4);
    // Survivors keep their letters: shell addresses them by index, so none may shift up.
    expect(left.map((l) => l.textContent)).toEqual(
      LETTERS.slice(0, count).filter((_, i) => ![AT.card, AT.approve, AT.deny, AT.reason].includes(i)),
    );
  });

  /** Panel round 2 (plan Task 12) removed the eight resume rows this test used to label -- the
   *  dashboard's own items replace them, and they are deliberately not `nav.ts` stops (`Dashboard`'s
   *  own doc comment: `controlsOf`'s selector has no `[role="button"]` branch, so a `dash` stop
   *  with no other control inside it drops out of `stopsIn`). The empty tab's composer is marked a
   *  HINT target too (`Composer`'s `hintTarget` prop), but jsdom reports it as zero-size unless
   *  `layOut` boxes it, and `layOut`'s own selector list (this describe block's own, above) has no
   *  `textarea` in it -- so this screen now offers `f` no target at all in this harness, which is
   *  the real, if narrow, thing left to pin down here. */
  it("offers no HINT targets on the empty tab's dashboard, where there is no conversation yet", () => {
    const { container } = render(<App />);
    dispatch({
      kind: "hello",
      ...HELLO,
      resumableSessions: [
        { provider: "claude", providerSessionId: "aaaaaaaa0000000000", createdAt: "1", updatedAt: "2" },
        { provider: "claude", providerSessionId: "bbbbbbbb0000000000", createdAt: "1", updatedAt: "2" },
      ],
    });
    dispatchEmptyTab();
    layOut(container);
    expect(container.querySelector(".dashboard")).not.toBeNull();
    const count = collect(1);
    expect(count).toBe(0);
    show(1, count);
    expect(labels(container)).toHaveLength(0);
  });

  /* K07 (2026-09-29): (a) a label is placed on the VISIBLE part of its target, its top clamped into the
     clipping box, so a code block whose top is scrolled out (or a sliver at either edge) keeps its label
     on screen; (b) a landing on a code block is drawn -- the block outlined, the row's sign hollow --
     until the next key, as the one-solid-mark rule draws a control inside the current row. The list
     is 0..1000 here (`layOut`), the root too, so a label's `top` is also its y in the list. */
  describe("K07: code block labels and landings", () => {
    const codeLabel = (c: HTMLElement) => c.querySelector<HTMLElement>(".hint-label.hint-code")!;
    const topOf = (el: HTMLElement) => parseFloat(el.style.top);
    function labelled(setUp: (c: HTMLElement) => void, replyTail?: string) {
      const rendered = conversation(replyTail);
      setUp(rendered.container);
      show(1, collect(1));
      return rendered;
    }

    it("K07-1: a block whose top is scrolled out gets its label at the visible top, not above the list", () => {
      const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, -62, 300));
      expect(codeLabel(container).style.top).toBe("4px");
    });
    it("K07-2: a block scrolled 3724px out of its top (the P6 case) likewise", () => {
      const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, -3724, 4000));
      expect(codeLabel(container).style.top).toBe("4px");
    });
    it("K07-3: a block fully in view keeps the label where it always was", () => {
      const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, 200, 50));
      expect(codeLabel(container).style.top).toBe("204px");
    });
    it("K07-4a: a sliver at the top edge: the label is inside the list, never above it", () => {
      const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, -290, 300));
      expect(codeLabel(container).style.top).toBe("4px");
    });
    it("K07-4b: a sliver at the bottom edge: the label is inside the list, never past it", () => {
      const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, 990, 300));
      expect(codeLabel(container).style.top).toBe("984px");
    });
    it("K07-5: a tall row whose sign cell starts above the list gets its label inside the list", () => {
      const { container } = labelled((c) => {
        const reply = c.querySelector<HTMLElement>(".row-assistant")!;
        box(reply, -500, 2000);
        box(reply.querySelector(".row-sign")!, -500, 2000);
      });
      const rowLabels = Array.from(container.querySelectorAll<HTMLElement>(".hint-label.hint-row"));
      expect(rowLabels.length).toBeGreaterThan(0);
      for (const label of rowLabels) expect(topOf(label)).toBeGreaterThanOrEqual(0);
    });
    it("K07-9: a link partly scrolled out gets its label inside the list", () => {
      const { container } = labelled(
        (c) => box(c.querySelector('.row-assistant a[href^="https:"]')!, -5, 20),
        "then see [the docs](https://example.com/docs).",
      );
      const link = container.querySelector<HTMLElement>(".hint-label.hint-link")!;
      expect(link).not.toBeNull();
      expect(topOf(link)).toBeGreaterThanOrEqual(0);
    });

    /** Lands HINT session `sessionId` on target `index`. */
    function land(sessionId: number, index: number) {
      show(sessionId, collect(sessionId));
      dispatch({ kind: "hint_land", sessionId, index });
    }
    const marked = (c: HTMLElement) => Array.from(c.querySelectorAll("pre[data-hint-landed]"));
    const listMarked = (c: HTMLElement) => c.querySelector(".message-list")!.hasAttribute("data-code-landed");

    it("K07-6: a landing on a code block marks that block and the list, and nothing else", () => {
      const { container } = conversation("then look.\n\n```\npwd\n```\n");
      const [first, second] = Array.from(container.querySelectorAll("pre.code-block"));
      expect(second).toBeDefined();
      land(1, AT.code);
      expect(marked(container)).toEqual([first]);
      expect(listMarked(container)).toBe(true);
    });
    it.each([
      ["j", (c: HTMLElement) => fireEvent.keyDown(root(c), { key: "j" })],
      ["y, after copying the block", (c: HTMLElement) => {
        const clipboard = stubClipboard();
        fireEvent.keyDown(root(c), { key: "y" });
        expect(clipboard.writeText).toHaveBeenCalledWith("ls -la");
      }],
      ["a new landing on a row", () => land(2, AT.prompt)],
      ["a new landing on a control", () => land(2, AT.approve)],
    ])("K07-7: the mark goes with the landing: %s", (_name, next) => {
      const { container } = conversation();
      land(1, AT.code);
      expect(marked(container)).toHaveLength(1);
      next(container);
      expect(marked(container)).toEqual([]);
      expect(listMarked(container)).toBe(false);
    });
    it("K07-10: a streamed delta that re-renders the landed block takes the hollow sign with it", () => {
      const { container } = started();
      events(
        { type: "user_prompt_submitted", text: "list it" },
        { type: "turn_started", turn_id: "t1" },
        { type: "content_delta", turn_id: "t1", kind: "text", text: "Run this:\n\n```\nls -la\n```\n\n" },
      );
      layOut(container);
      act(() => root(container).focus());
      land(1, 2); // prompt 0, the reply 1, its code block 2
      expect(listMarked(container)).toBe(true);
      const block = container.querySelector("pre.code-block");
      dispatch({
        kind: "events", tab: 1, fromRevision: 3, throughRevision: 4,
        events: [{ type: "content_delta", turn_id: "t1", kind: "text", text: "then look." }],
      });
      expect(block!.isConnected).toBe(false);
      expect(marked(container)).toEqual([]);
      expect(listMarked(container)).toBe(false);
    });
    it("K07 fix round, Codex 2: an arrival that lands on a card takes the block's mark with it", () => {
      const { container } = conversation();
      land(1, AT.code);
      expect(listMarked(container)).toBe(true);
      dispatch({ kind: "pane_focus", focused: false });
      dispatch({ kind: "pane_focus", focused: true });
      dispatch({ kind: "arrive" });
      expect(container.querySelector(".row-current")).toBe(container.querySelector(".row-permission"));
      expect(marked(container)).toEqual([]);
      expect(listMarked(container)).toBe(false);
    });
    it("K07 fix round: an arrival that stays on the block's row keeps the mark (y still copies it)", () => {
      const { container } = started();
      events(
        { type: "user_prompt_submitted", text: "list it" },
        { type: "turn_started", turn_id: "t1" },
        { type: "content_delta", turn_id: "t1", kind: "text", text: "Run this:\n\n```\nls -la\n```\n\n" },
      );
      layOut(container);
      act(() => root(container).focus());
      land(1, 2);
      dispatch({ kind: "pane_focus", focused: false });
      dispatch({ kind: "pane_focus", focused: true });
      dispatch({ kind: "arrive" });
      expect(marked(container)).toHaveLength(1);
      expect(listMarked(container)).toBe(true);
    });
    it("K07 fix round, Codex 3: the clamp uses the label's real height (a large font)", () => {
      const tall = vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
        return this.classList.contains("hint-label") ? 31 : 0;
      });
      try {
        const { container } = labelled((c) => box(c.querySelector("pre.code-block")!, 990, 300));
        expect(codeLabel(container).style.top).toBe("969px");
      } finally {
        tall.mockRestore();
      }
    });
    it("K07 fix round, Codex 4: a link scrolled sideways out of its box keeps its label inside it", () => {
      const { container } = labelled(
        (c) => {
          const a = c.querySelector<HTMLElement>('.row-assistant a[href^="https:"]')!;
          a.getBoundingClientRect = () =>
            ({ top: 100, bottom: 120, left: -50, right: 50, width: 100, height: 20, x: -50, y: 100 }) as DOMRect;
        },
        "then see [the docs](https://example.com/docs).",
      );
      const link = container.querySelector<HTMLElement>(".hint-label.hint-link")!;
      expect(parseFloat(link.style.left)).toBeGreaterThanOrEqual(0);
    });
    it("K07-8: a landing on a block in another row reveals the block, not the row", () => {
      const { container } = conversation();
      expect(container.querySelector(".row-current .code-block")).toBeNull();
      const reveal = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
      show(1, collect(1));
      reveal.mockClear();
      dispatch({ kind: "hint_land", sessionId: 1, index: AT.code });
      const block = container.querySelector("pre.code-block")!;
      expect(reveal.mock.contexts).toContain(block);
      expect(reveal.mock.contexts).not.toContain(block.closest(".row"));
    });
  });
});

/* The owner, on an installed build (2026-09-19): "现在jk没法在选中输出的时候滚动屏幕，特别是在最后
   输出很长的时候，没法滚动看下面的". `j`/`k` moved row to row only, so a reply taller than the view was
   skipped over, and on the LAST row there was nowhere to move and the rest of it was unreachable.

   jsdom implements no layout, so `fakeLayout` stands one in: the rows stacked at the given heights in
   a list viewport `viewport` px tall at y = 0, every rect following the list's `scrollTop`, which
   clamps to [0, scrollHeight - clientHeight] the way a browser's does. Each row's line height is set
   inline to 20px, so one step is 3 x 20 = 60px. None of this proves what a real WebKit draws. */
describe("App keyboard: scrolling through the conversation", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  function prompts(...texts: string[]) {
    events(...texts.map((text): AgentDomainEvent => ({ type: "user_prompt_submitted", text })));
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string, over: { ctrlKey?: boolean; shiftKey?: boolean } = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...over });
  const current = (c: HTMLElement) => c.querySelector(".row-current .row-body")!.textContent;

  function fakeLayout(container: HTMLElement, heights: number[], viewport = 400) {
    const list = container.querySelector(".message-list") as HTMLElement;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    expect(rows.length).toBe(heights.length);
    const total = heights.reduce((a, b) => a + b, 0);
    let scrollTop = 0;
    Object.defineProperty(list, "clientHeight", { value: viewport, configurable: true });
    Object.defineProperty(list, "scrollHeight", { value: total, configurable: true });
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => scrollTop,
      set: (v: number) => {
        scrollTop = Math.max(0, Math.min(total - viewport, v));
      },
    });
    list.getBoundingClientRect = () => ({ top: 0, bottom: viewport }) as DOMRect;
    let y = 0;
    rows.forEach((row, i) => {
      const top = y;
      y += heights[i];
      row.getBoundingClientRect = () => ({ top: top - scrollTop, bottom: top + heights[i] - scrollTop }) as DOMRect;
      // Codex review, v1 trial item 5 fix round, finding 4: the real `.row` wrapper (`index.css`)
      // sets neither `font-size` nor `line-height` -- only `.row-body` (`Row.tsx`'s own text cell)
      // does (`--fs-prose` / `1.65`). Deliberately mismatched from the row's own 20px, so a
      // regression back to measuring the wrapper fails loudly rather than by coincidence agreeing.
      row.style.lineHeight = "999px";
      const body = row.querySelector<HTMLElement>(".row-body")!;
      body.style.lineHeight = "20px";
    });
    act(() => root(container).focus());
    return list;
  }

  it("j scrolls through a tall middle row, landing on its top, and moves on only once its end is on screen", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    // R1 lands the cursor on "c" the moment it arrives; `gg` returns to the top.
    press("g");
    press("g");

    press("j");
    expect(current(container)).toBe("b");
    // The tall row's top was already on screen (at 100): landing moves NOTHING, so the tail of "a"
    // just read stays in view (review: aligning it jumped the view by a quarter of it here).
    expect(list.scrollTop).toBe(0);

    // 690px of it is below the view: eleven 60px steps, then a 30px one -- never past the edge.
    const seen: number[] = [];
    for (let i = 0; i < 12; i++) {
      press("j");
      seen.push(list.scrollTop);
    }
    expect(current(container)).toBe("b");
    expect(seen).toEqual([60, 120, 180, 240, 300, 360, 420, 480, 540, 600, 660, 690]);
    expect(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]')[1].getBoundingClientRect().bottom).toBe(400);

    press("j");
    expect(current(container)).toBe("c");
  });

  it("a scroll step hands the keys back to the row when a control inside it had them", () => {
    // After `l` onto a tall card's Approve, `j` scrolls the card. Left on Approve, the keys would
    // follow a button that is scrolling off screen and still answers Enter.
    const { container } = started();
    events(
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    const list = fakeLayout(container, [100, 990]);
    press("j");
    press("l");
    expect(document.activeElement?.textContent).toBe("Approve");
    press("j");
    expect(list.scrollTop).toBeGreaterThan(0);
    expect(document.activeElement).toBe(root(container));
  });

  it("j reads a tall LAST row to its end, then stops", () => {
    const { container } = started();
    prompts("a", "b");
    const list = fakeLayout(container, [100, 990]);

    for (let i = 0; i < 30; i++) press("j");

    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(690); // the very end of the list
  });

  it("K01: a pane switch drops a count, as it drops a prefix", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3");
    press("g"); press("g"); press("3");
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    press("j");
    expect(current(container)).toBe("r1");
  });
  /* Fix round 1 (review): Shift+Tab never reaches `onKeyDown` (`onModeKey` stops it), so it drops a
     waiting prefix and a count itself -- the mode still cycles. */
  it("K01: Shift+Tab drops a waiting prefix: g, Shift+Tab, g is no gg", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3");
    press("k"); press("k");
    expect(current(container)).toBe("r1");
    press("g");
    press("Tab", { shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    press("g");
    expect(current(container)).toBe("r1");
  });
  it("K01: Shift+Tab drops a count: 2, Shift+Tab, j moves one row", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3");
    press("g"); press("g");
    expect(current(container)).toBe("r0");
    press("2");
    press("Tab", { shiftKey: true });
    press("j");
    expect(current(container)).toBe("r1");
  });
  /* Fix round 1 (review): R2, a count survives its prefix -- `case "pending"` keeps it for the second
     key, and `prompt-jump` repeats (before K01, `3[[` jumped one prompt). */
  it("K01 (R2): a count survives its prefix: 3[[ goes three prompts back, 2]] two forward", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4", "r5");
    expect(current(container)).toBe("r5");
    press("3"); press("["); press("[");
    expect(current(container)).toBe("r2");
    press("2"); press("]"); press("]");
    expect(current(container)).toBe("r4");
  });
  /* K01 fix round 2 (review): `[`/`]` arm with Alt or Meta held, so their second key reads them the
     same way and `[[`/`]]` typed with either on both keys still jump, as before K01 -- on a layout
     that types a bracket with Option (macOS German, French, Swiss), both keys are typed that way. */
  it("K01: [[ and ]] typed with Alt or Meta held on both keys still jump", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3");
    expect(current(container)).toBe("r3");
    const held = (key: string, over: { altKey?: boolean; metaKey?: boolean }) =>
      fireEvent.keyDown(document.activeElement ?? document.body, { key, ...over });
    held("[", { altKey: true }); held("[", { altKey: true });
    expect(current(container)).toBe("r2");
    held("[", { metaKey: true }); held("[", { metaKey: true });
    expect(current(container)).toBe("r1");
    held("]", { altKey: true }); held("]", { altKey: true });
    expect(current(container)).toBe("r2");
    held("]", { metaKey: true }); held("]", { metaKey: true });
    expect(current(container)).toBe("r3");
  });

  /* Task 3 (v1 picks, R2): a count before `G`/`gg` is vim's `{N}G`/`{N}gg` (`:help G`, `:help gg`) --
     row N, and before `gt`/`gT` (`:help gt`) the tab NUMBERED N / N tabs back. Every other pair
     ignores it, and `[[`/`]]` already repeat it (K01's own test above). */
  it("counts: 3G and 3gg go to row 3, 2]] two prompts on (R2)", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    press("3"); press("G", { shiftKey: true }); expect(current(container)).toBe("r2");
    press("g"); press("g"); expect(current(container)).toBe("r0");
    press("3"); press("g"); press("g"); expect(current(container)).toBe("r2");
    press("g"); press("g"); press("2"); press("]"); press("]"); expect(current(container)).toBe("r2");
  });

  it("counts: 2gt goes to tab 2, 9gt says there is none, a bare gt still steps (R2)", () => {
    const widen = stubBandWidth();
    try {
      const { container } = started();
      act(() => widen(container));
      dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }, { ...LIVE_TAB, id: 3, number: 3, label: "3 new" }] });
      const pair = (k: string, action: string) => ({ keys: ["g", k], action, desc: action, source: "default" });
      dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], newTabChord: "Ctrl+b c",
        panel: { leader: " ", leaderLabel: "Space", leaderSource: "default", timeoutlen: 1000, timeout: true, groups: [], bindings: [pair("t", "tab.next"), pair("T", "tab.prev")] } });
      posted = [];
      press("2"); press("g"); press("t");
      expect(lastOfType("select_tab")).toMatchObject({ tab: 2 });
      expect(lastOfType("tab_verb")).toBeUndefined();
      posted = [];
      press("9"); press("g"); press("t");
      expect(posted).toEqual([]);
      expect(container.querySelector(".band-message")?.textContent).toBe("no tab 9");
      press("g"); press("t");
      expect(lastOfType("tab_verb")).toMatchObject({ verb: "next" });
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("counts: a count past the last row lands on the last row, and 1G returns to the first", () => {
    const { container } = started();
    prompts("r0", "r1", "r2");
    press("g"); press("g");
    expect(current(container)).toBe("r0");
    // A real capital G puts a bare Shift keydown before it; that must not drop the count.
    press("9"); press("9"); press("Shift", { shiftKey: true }); press("G", { shiftKey: true });
    expect(current(container)).toBe("r2");
    press("g"); press("g");
    press("9"); press("g"); press("g");
    expect(current(container)).toBe("r2");
    press("1"); press("G", { shiftKey: true });
    expect(current(container)).toBe("r0");
  });

  it("counts: {N}G brings row N on screen the way a j landing does, and never scrolls the list to an end", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [400, 990, 400]);
    press("g"); press("g");
    expect(list.scrollTop).toBe(0);
    // "b" is taller than the 400px view and starts where the view ends: its top is aligned, as `j` does.
    press("2"); press("G", { shiftKey: true });
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(400);
    // A bare `G` still scrolls the list to its very end ...
    press("G", { shiftKey: true });
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBe(1390);
    // ... and `2gg` back onto "b" shows the edge it arrives from (its bottom), not the top of the list.
    press("2"); press("g"); press("g");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(990);
  });

  it("counts: {N}G onto a row that fits is revealed with scrollIntoView, leaving the list where it was", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [400, 990, 400]);
    press("g"); press("g");
    expect(list.scrollTop).toBe(0);
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();
    press("3"); press("G", { shiftKey: true });
    expect(current(container)).toBe("c");
    // The reveal is the cursor effect's -- the same call a `j` landing makes on a row that fits ...
    expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
    // ... and a bare `G` would have scrolled the list to its very end (1390) before it.
    expect(list.scrollTop).toBe(0);
  });

  it("counts: 3gt posts the tab's id, 2gT wraps back past the first tab, and a count before ]b is ignored (R2)", () => {
    started();
    const info = (id: number, number: number) => ({ ...LIVE_TAB, id, number, label: `${number} new` });
    dispatch({ kind: "tabs", active: 1, tabs: [info(1, 1), info(2, 2), info(5, 3)] });
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], newTabChord: "Ctrl+b c",
      panel: { ...TABLE, bindings: [binding(["g", "t"], "tab.next"), binding(["g", "T"], "tab.prev"), binding(["]", "b"], "tab.next")] } });
    posted = [];
    press("3"); press("g"); press("t");
    // The third tab in the bar is tab id 5: the message names the id, never the number.
    expect(lastOfType("select_tab")).toMatchObject({ tab: 5 });
    expect(lastOfType("tab_verb")).toBeUndefined();
    posted = [];
    // A real `gT` puts a bare Shift keydown between `g` and `T`; it must not drop the count.
    press("2"); press("g"); press("Shift", { shiftKey: true }); press("T", { shiftKey: true });
    expect(lastOfType("select_tab")).toMatchObject({ tab: 2 });
    expect(lastOfType("tab_verb")).toBeUndefined();
    posted = [];
    press("3"); press("]"); press("b");
    expect(posted.filter((m) => m.type === "tab_verb").map((m) => m.verb)).toEqual(["next"]);
    expect(lastOfType("select_tab")).toBeUndefined();
  });

  it("k is the mirror: lands on a tall row's BOTTOM and scrolls up through it before moving", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    // R1 lands the cursor on "c" the moment it arrives; `gg` returns to the top.
    press("g");
    press("g");
    for (let i = 0; i < 14; i++) press("j");
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBe(690);
    // "c" fits, so landing on it used `scrollIntoView({ block: "nearest" })` -- a mock here. Do what
    // a browser would: bring "c" fully on screen, which is also the end of the list.
    list.scrollTop = 790;

    press("k");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(790); // its BOTTOM was already on screen (at 300): nothing moves

    const seen: number[] = [];
    for (let i = 0; i < 12; i++) {
      press("k");
      seen.push(list.scrollTop);
    }
    expect(seen).toEqual([730, 670, 610, 550, 490, 430, 370, 310, 250, 190, 130, 100]);
    expect(current(container)).toBe("b");

    press("k");
    expect(current(container)).toBe("a");
  });

  it("j onto a tall row whose top is NOT on screen aligns that top with the view's top; k mirrors it", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [400, 990, 400]);
    // R1 lands the cursor on "c" the moment it arrives; `gg` returns to the top.
    press("g");
    press("g");

    press("j"); // "b" starts at 400, exactly where the 400px view ends
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(400);

    press("G", { shiftKey: true }); // "c", view at the very end (1390)
    expect(list.scrollTop).toBe(1390);
    press("k"); // "b" ends at 1390, exactly where the view begins
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(990); // its bottom at the bottom of the view
  });

  it("from the Stop button, k moves back to the row rather than scrolling it", () => {
    const { container } = started({ capabilities: { ...initialState().capabilities, interrupt: true } });
    events({ type: "turn_started", turn_id: "t1" });
    prompts("a", "b");
    act(() => root(container).focus());
    press("j"); // no layout yet: a plain move onto "b"
    const list = fakeLayout(container, [100, 1000]);
    list.scrollTop = 300; // "b" runs past both edges
    const stop = buttonLabelled(container, "ctrl+c interrupt")!;
    act(() => stop.focus());

    press("k");

    expect(document.activeElement).toBe(root(container));
    expect(list.scrollTop).toBe(300);
    expect(current(container)).toBe("b");
  });

  it("Ctrl+d and Ctrl+u scroll half a view, and bring the cursor to a row that is still on screen", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    // R1 lands the cursor on "r4" the moment it arrives; `gg` returns to the top.
    press("g");
    press("g");
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;

    press("d", { ctrlKey: true });
    expect(list.scrollTop).toBe(200);
    expect(current(container)).toBe("r0"); // still partly on screen: the cursor stays

    scrollIntoView.mockClear();
    press("d", { ctrlKey: true });
    expect(list.scrollTop).toBe(400);
    expect(current(container)).toBe("r1"); // r0 left the view; r1 is the first row on screen
    expect(list.scrollTop).toBe(400); // and the view stayed where Ctrl+d put it
    expect(scrollIntoView).not.toHaveBeenCalled();

    press("G", { shiftKey: true }); // cursor to r4, the view at the very end
    expect(current(container)).toBe("r4");
    expect(list.scrollTop).toBe(1100);
    press("u", { ctrlKey: true });
    expect(list.scrollTop).toBe(900);
    expect(current(container)).toBe("r4");
    press("u", { ctrlKey: true });
    expect(list.scrollTop).toBe(700);
    expect(current(container)).toBe("r3"); // the LAST row still on screen
  });

  it("Ctrl+d brings the cursor to the visible row NEAREST its own, not to the first one on screen", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7");
    const list = fakeLayout(container, [100, 100, 100, 100, 100, 100, 100, 100], 250);
    press("G", { shiftKey: true }); // cursor on r7, at the end
    expect(current(container)).toBe("r7");
    list.scrollTop = 0; // the wheel took the view back up: r0-r2 on screen, r7 far below

    press("d", { ctrlKey: true }); // +125: r1-r3 on screen, r7 still below
    expect(list.scrollTop).toBe(125);
    expect(current(container)).toBe("r3"); // the last row on screen -- not r1, many rows UP
  });

  it("Ctrl+d that re-homes the cursor takes the keys back from a focused control", () => {
    const { container } = started();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    prompts("r1", "r2", "r3");
    const list = fakeLayout(container, [300, 300, 300, 300]);
    // R1 lands the cursor on "r3" the moment it arrives; `gg` returns to the permission card at row 0.
    press("g");
    press("g");
    const rows = list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
    expect(rows[0].querySelector('[data-nav-action="allow"]')).not.toBeNull();
    press("l"); // Approve on the card (row 0) has the keys
    expect((document.activeElement as HTMLElement).closest('[data-nav-stop="row"]')).toBe(rows[0]);

    press("d", { ctrlKey: true });
    press("d", { ctrlKey: true }); // the card has left the view: the cursor re-homes to r1
    expect(current(container)).toBe("r1");
    // The root, so Enter now acts on r1 rather than natively activating an Approve nobody can see
    // (jsdom performs no native activation, so this focus is the observable; Enter is not pressed).
    expect(document.activeElement).toBe(root(container));
  });

  it("Ctrl+d never answers a permission, whatever the plain d does", () => {
    const { container } = started();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    act(() => root(container).focus());
    press("d", { ctrlKey: true });
    expect(lastOfType("permission_response")).toBeUndefined();
  });

  /* v1 trial item 5 (owner: "能不能给browse 加上contrl e/y", copying vim's own `:help CTRL-E`/
     `:help CTRL-Y`): one text line -- `fakeLayout`'s own 20px, the same line `rowScrollStep`'s
     three-line tall-row step is built from -- not half a view. */
  it("Ctrl+e and Ctrl+y scroll by one text line, and the cursor stays while its row is on screen", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("e", { ctrlKey: true });
    expect(list.scrollTop).toBe(20);
    expect(current(container)).toBe("r0"); // nowhere near leaving the view

    press("e", { ctrlKey: true });
    expect(list.scrollTop).toBe(40);

    press("y", { ctrlKey: true });
    expect(list.scrollTop).toBe(20);
    press("y", { ctrlKey: true });
    expect(list.scrollTop).toBe(0);
  });

  /* Codex review, v1 trial item 5 fix round, finding 4: the one-line step must measure the text the
     reader actually sees (`.row-body`'s own computed line height), not the sign-column grid wrapper
     around it (`.row`, which `index.css` never gives a font-size or line-height of its own -- it
     inherits whatever is ambient, `line-height: normal`, well under `.row-body`'s real 15px x 1.65 =
     24.75px). `fakeLayout` now pins this directly: the row's own line-height (999px) would make one
     press jump nearly to the list's end; `.row-body`'s (20px) is what every other test here expects. */
  it("Ctrl+e steps by the row's TEXT line height, not the sign-column wrapper's own", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("e", { ctrlKey: true });

    expect(list.scrollTop).toBe(20); // `.row-body`'s 20px -- not the row wrapper's mismatched 999px
  });

  it("a count repeats Ctrl+e/Ctrl+y, capped the same way every other count is (R4)", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("5");
    press("e", { ctrlKey: true });
    expect(list.scrollTop).toBe(100); // five 20px lines in one press
    expect(current(container)).toBe("r0"); // row 0 (0-300) still overlaps the 400px view

    press("1");
    press("0");
    press("0"); // "100"
    press("y", { ctrlKey: true });
    expect(list.scrollTop).toBe(0); // clamped at the top, exactly like an ordinary scrollTop write
  });

  /* Codex review, v1 trial item 5 fix round, finding 1: a real `Ctrl+e` arrives as two keydowns --
     `Control` on its own (with `ctrlKey: true` already set on that very event, as every browser
     reports it), THEN `e` -- not one keydown carrying both. `press("e", { ctrlKey: true })` alone
     (the test above) never dispatches the first one, so it could not have caught the guard clearing
     `countRef` on that bare `Control` keydown before `e` ever reads it back. */
  it("a bare Control keydown between the count and Ctrl+e does not clear the count", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("5");
    press("Control", { ctrlKey: true });
    press("e", { ctrlKey: true });
    expect(list.scrollTop).toBe(100); // still five 20px lines, exactly like the test above

    press("2");
    press("0");
    press("Control", { ctrlKey: true });
    press("y", { ctrlKey: true });
    expect(list.scrollTop).toBe(0); // clamped at the top -- twenty 20px lines up from 100
  });

  it("Ctrl+e that carries the cursor's row off screen re-homes it to the nearest visible row", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("2");
    press("0");
    press("e", { ctrlKey: true }); // 20 lines * 20px = 400px: r0 (0-300) leaves the view entirely
    expect(list.scrollTop).toBe(400);
    expect(current(container)).toBe("r1"); // the first row still on screen -- the same rule Ctrl+d uses

    press("G", { shiftKey: true }); // cursor to r4, the view at the very end
    expect(current(container)).toBe("r4");
    press("2");
    press("0");
    press("y", { ctrlKey: true }); // 400px back up: r4 (1200-1500) leaves the view entirely
    expect(current(container)).toBe("r3"); // the LAST row still on screen
  });

  /* v1 picks, Task 5 (decision #13, ruling R10; vim `:help CTRL-F`): a whole view down or up with two
     text lines of the old one kept, counted. Rows of 300 in a 400px view and 20px lines, so one page is
     400 - 2 x 20 = 360px. */
  it("Ctrl+f, PageDown and PageUp scroll a view keeping two lines; a count repeats them", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    list.style.lineHeight = "20px";
    press("g");
    press("g");

    press("f", { ctrlKey: true });
    expect(list.scrollTop).toBe(360);
    press("PageDown");
    expect(list.scrollTop).toBe(720);
    press("PageUp");
    expect(list.scrollTop).toBe(360);
    press("PageUp");
    expect(list.scrollTop).toBe(0);

    press("2");
    press("f", { ctrlKey: true });
    expect(list.scrollTop).toBe(720); // two pages in one press
    press("2");
    press("PageUp");
    expect(list.scrollTop).toBe(0);
    press("2");
    press("PageDown");
    expect(list.scrollTop).toBe(720);
  });

  /* The same trap Ctrl+e's own test names: a real `Ctrl+f` is a bare `Control` keydown FIRST, then `f`,
     and that first key must not spend the count. */
  it("a bare Control keydown between the count and Ctrl+f does not clear the count", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");

    press("2");
    press("Control", { ctrlKey: true });
    press("f", { ctrlKey: true });
    expect(list.scrollTop).toBe(720);
  });

  /* Two lines of what the reader READS: the cursor row's text (`.row-body`, which `fakeLayout` gives
     20px), never the list's own line height -- `.message-list` sets none in `index.css`, so it reads
     `normal`, well under the prose line (the finding Ctrl+e's own text-line test records). The list's
     mismatched 999px here would leave one pixel of a page. */
  it("a page keeps two lines of the row's TEXT, not of the list's own line height", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    list.style.lineHeight = "999px";
    press("g");
    press("g");

    press("f", { ctrlKey: true });

    expect(list.scrollTop).toBe(360);
  });

  /* A page never goes against its own direction. In a view shorter than the two lines it keeps (30px of
     view, 20px lines: 30 - 40 = -10) a bare subtraction would make Ctrl+f scroll UP; the step is at
     least one pixel instead, so the key still goes forward and PageUp still comes back. */
  it("a page in a view shorter than its two kept lines still goes the way the key says", () => {
    const { container } = started();
    prompts("r0", "r1", "r2");
    const list = fakeLayout(container, [300, 300, 300], 30);
    press("g");
    press("g");

    press("f", { ctrlKey: true });
    expect(list.scrollTop).toBe(1);
    press("PageDown");
    expect(list.scrollTop).toBe(2);
    press("PageUp");
    expect(list.scrollTop).toBe(1);
  });

  /* The cursor follows the view the way Ctrl+d's does: a row still (partly) on screen keeps it, one that
     left is replaced by the NEAREST row on screen, and the view stays where the page put it. */
  it("a page that carries the cursor's row off screen re-homes it to the nearest visible row", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();

    press("f", { ctrlKey: true }); // 360: r0 (0-300) has left; r1 (300-600) is the first row on screen
    expect(list.scrollTop).toBe(360);
    expect(current(container)).toBe("r1");
    press("PageDown"); // 720: r1 has left; r2 (600-900) is the first on screen
    expect(list.scrollTop).toBe(720);
    expect(current(container)).toBe("r2");
    expect(scrollIntoView).not.toHaveBeenCalled(); // and the view stayed where the page put it
    press("PageUp"); // 360: r2 is still partly on screen: the cursor stays
    expect(list.scrollTop).toBe(360);
    expect(current(container)).toBe("r2");

    press("G", { shiftKey: true }); // cursor to r4, the view at the very end (1100)
    expect(current(container)).toBe("r4");
    press("PageUp"); // 740: r4 (1200-1500) is below the view; r3 (900-1200) is the LAST row on screen
    expect(list.scrollTop).toBe(740);
    expect(current(container)).toBe("r3");
  });

  it("a page that re-homes the cursor takes the keys back from a focused control", () => {
    const { container } = started();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    prompts("r1", "r2", "r3");
    const list = fakeLayout(container, [300, 300, 300, 300]);
    press("g");
    press("g");
    const rows = list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
    expect(rows[0].querySelector('[data-nav-action="allow"]')).not.toBeNull();
    press("l"); // Approve on the card (row 0) has the keys
    expect((document.activeElement as HTMLElement).closest('[data-nav-stop="row"]')).toBe(rows[0]);

    press("f", { ctrlKey: true }); // 360: the card has left the view; the cursor re-homes to r1
    expect(current(container)).toBe("r1");
    // The root, so Enter now acts on r1 rather than natively activating an Approve nobody can see.
    expect(document.activeElement).toBe(root(container));
  });

  /* The announcement the follow logic reads (`./follow`): a page down leaves the scroll's own
     direction to decide (reaching the bottom re-arms following), a page up stops following at once. */
  it("a page announces the scroll it makes: down for Ctrl+f and PageDown, up for PageUp", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g");
    press("g");
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));

    press("f", { ctrlKey: true });
    press("PageDown");
    press("PageUp");

    expect(seen).toEqual(["down", "down", "up"]);
  });

  it("a page with no rows at all does nothing and breaks nothing", () => {
    const { container } = started();
    const at = root(container);
    expect(() => {
      fireEvent.keyDown(at, { key: "f", ctrlKey: true });
      fireEvent.keyDown(at, { key: "PageDown" });
      fireEvent.keyDown(at, { key: "PageUp" });
    }).not.toThrow();
    expect(container.querySelector(".row-current")).toBeNull();
  });

  /* ArrowDown/ArrowUp are exactly `j`/`k` (ruling R10): the same action, so the same row-at-a-time
     move, the same count, and the same walk through a row taller than the view. */
  it("ArrowDown and ArrowUp move a row like j and k, and take a count", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3");
    fakeLayout(container, [100, 100, 100, 100]);
    press("g");
    press("g");

    press("ArrowDown");
    expect(current(container)).toBe("r1");
    press("ArrowDown");
    expect(current(container)).toBe("r2");
    press("ArrowUp");
    expect(current(container)).toBe("r1");
    press("2");
    press("ArrowDown");
    expect(current(container)).toBe("r3");
    press("3");
    press("ArrowUp");
    expect(current(container)).toBe("r0");
  });

  it("ArrowDown scrolls through a tall row before moving on, exactly as j does", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");

    press("ArrowDown");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(0);
    // 690px of "b" is below the view: eleven 60px steps, then a 30px one -- j's own numbers.
    const seen: number[] = [];
    for (let i = 0; i < 12; i++) {
      press("ArrowDown");
      seen.push(list.scrollTop);
    }
    expect(current(container)).toBe("b");
    expect(seen).toEqual([60, 120, 180, 240, 300, 360, 420, 480, 540, 600, 660, 690]);
    press("ArrowDown");
    expect(current(container)).toBe("c");
  });

  it("G goes to the last row and the very end of the list; gg to the first row and the top", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);

    press("G", { shiftKey: true });
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBe(790);

    press("g");
    press("g");
    expect(current(container)).toBe("a");
    expect(list.scrollTop).toBe(0);
  });

  it("G on a long last reply shows its end, not its top", () => {
    const { container } = started();
    prompts("a", "b");
    const list = fakeLayout(container, [100, 990]);
    press("G", { shiftKey: true });
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(690);
  });

  /* v1 picks, Task 4 (vim `:help zt`/`zz`/`zb`): the ROW goes to an edge of the view -- not a text
     line -- and with a count, row N first (`3zt`: row 3 at the top, and the cursor on it). Rows of 300
     in a 400px view, so r2 (600-900) is never on an edge by luck. */
  it("zt / zb / zz put the cursor's row at the top / bottom / middle; 3zt row 3 at the top", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g"); press("2"); press("j"); // r2: top 600, view 400
    press("z"); press("t"); expect(list.scrollTop).toBe(600);
    press("z"); press("b"); expect(list.scrollTop).toBe(500);
    press("z"); press("z"); expect(list.scrollTop).toBe(550);
    press("g"); press("g"); press("3"); press("z"); press("t");
    expect(current(container)).toBe("r2");
    expect(list.scrollTop).toBe(600);
  });

  it("zt / zb / zz never move the cursor without a count, and take the keys back for the row", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g"); press("2"); press("j");
    expect(current(container)).toBe("r2");
    for (const second of ["t", "b", "z"]) {
      // The keys are somewhere else (a control inside the panel, say): like every cursor command, the
      // scroll hands them back to the row, or a stale focus would keep answering Enter for it.
      act(() => root(container).blur());
      expect(document.activeElement, `before z${second}`).not.toBe(root(container));
      fireEvent.keyDown(root(container), { key: "z" });
      fireEvent.keyDown(root(container), { key: second });
      expect(current(container), `z${second}`).toBe("r2");
      expect(document.activeElement, `z${second}`).toBe(root(container));
    }
    expect(list.scrollTop).toBe(550);
  });

  /* A row taller than the view is aligned by its own edge (its top for `zt`, its bottom for `zb`, its
     middle for `zz`), never refused; and an edge the list cannot reach is where scrollTop clamps, as a
     browser clamps it -- the first row's bottom above the view's bottom, the last row's top below the
     view's top. */
  it("zt / zb / zz align a row taller than the view, and clamp where the list ends", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]); // total 1190: scrollTop 0..790
    press("g"); press("g");
    press("z"); press("b");
    expect(list.scrollTop, "a's bottom (100) cannot go below the top of the list").toBe(0);
    press("j"); // "b": 100-1090
    expect(current(container)).toBe("b");
    press("z"); press("t"); expect(list.scrollTop).toBe(100);
    press("z"); press("b"); expect(list.scrollTop).toBe(690); // 1090 - 400
    press("z"); press("z"); expect(list.scrollTop).toBe(395); // its middle (595) at the view's middle (200)
    press("G", { shiftKey: true });
    expect(current(container)).toBe("c");
    press("z"); press("t");
    expect(list.scrollTop, "c's top (1090) is past what the list can scroll to").toBe(790);
    expect(current(container)).toBe("c");
  });

  /* R2: a count is a row number (1-based), clamped to the rows there are, and the cursor lands on it
     whichever edge it is put at. */
  it("a count before zt / zz / zb names the row: 9zt clamps to the last, 2zz and 4zb land on theirs", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g");
    press("9"); press("z"); press("t");
    expect(current(container)).toBe("r4");
    expect(list.scrollTop, "r4's top (1200) is past the end").toBe(1100);

    press("g"); press("g");
    press("2"); press("z"); press("z");
    expect(current(container)).toBe("r1");
    expect(list.scrollTop).toBe(250); // r1 300-600: its middle (450) at the view's middle (200)

    press("4"); press("z"); press("b");
    expect(current(container)).toBe("r3");
    expect(list.scrollTop).toBe(800); // r3 900-1200: its bottom at the view's bottom (400)
  });

  /* A row `zt` has just placed is not revealed again: the `[cursor]` effect that brings a moved cursor's
     row on screen would otherwise `scrollIntoView` it (`block: "nearest"`) over the edge the key chose --
     in jsdom that call is a mock, so what can be seen is whether it happened. */
  it("a counted zt places the row itself: nothing reveals it again afterwards", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g");
    const reveal = Element.prototype.scrollIntoView as unknown as ReturnType<typeof vi.fn>;
    reveal.mockClear();
    press("3"); press("z"); press("t");
    expect(current(container)).toBe("r2");
    expect(list.scrollTop).toBe(600);
    expect(reveal).not.toHaveBeenCalled();
  });

  it("3zt spends its count: the next j moves one row, not three", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g");
    press("3"); press("z"); press("t");
    expect(current(container)).toBe("r2");
    press("j");
    expect(current(container)).toBe("r3");
  });

  /* The announcement the follow logic reads (`./follow`): a scroll the panel makes itself says which
     way, so a `zb` that brings the view UP stops following at once instead of waiting for the scroll
     event, and a `zt` that brings it down leaves the scroll's own direction to decide. */
  it("zt / zb / zz announce the scroll they make: down when the view moves down, up when it moves up", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    const list = fakeLayout(container, [300, 300, 300, 300, 300]);
    press("g"); press("g"); press("2"); press("j");
    list.scrollTop = 0;
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    press("z"); // the first half says nothing
    expect(seen).toEqual([]);
    press("t"); // r2's top (600) is below the view's top: the view moves down (to 600)
    press("z"); press("b"); // its bottom (900) is above the view's bottom (1000): up (to 500)
    press("z"); press("z"); // its middle (750) is below the view's middle (700): down (to 550)
    expect(seen).toEqual(["down", "up", "down"]);
  });

  it("zt / zz / zb and a counted 3zt with no rows at all do nothing and break nothing", () => {
    const { container } = started();
    const at = root(container);
    expect(at).not.toBeNull();
    const keys = (...list: string[]) => list.forEach((key) => fireEvent.keyDown(at, { key }));
    expect(() => {
      keys("z", "t", "z", "z", "z", "b", "3", "z", "t");
    }).not.toThrow();
    expect(container.querySelector(".row-current")).toBeNull();
    // ...and the prefix was spent each time: the next key is a plain key again.
    keys("i");
    expect(container.querySelector("textarea")).not.toBeNull();
  });

  it("a lone g does nothing, and any other key cancels it", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 100, 100]);
    list.scrollTop = 0;
    // R1 lands the cursor on "c" the moment it arrives; move back to "a" with `k`, not `gg`, since
    // `gg` is the very mechanism this test is about.
    press("k");
    press("k");
    expect(current(container)).toBe("a");

    press("g");
    expect(current(container)).toBe("a");

    // K01 (X-A-11): the key that cancels a lone `g` is swallowed with it. This used to pin the leak
    // -- that first `j` moved as a bare `j` -- which is how `g`, a pause, `d` denied a card.
    press("j");
    expect(current(container)).toBe("a");
    press("j");
    press("j");
    expect(current(container)).toBe("c");
    press("g");
    press("x"); // not a key the table knows -- still cancels
    press("g");
    expect(current(container)).toBe("c");
    press("j"); // and a key it does know cancels too
    press("g");
    expect(current(container)).toBe("c");
  });

  it("a pane switch cancels a pending g, though the WebView saw no key for it", () => {
    const { container } = started();
    prompts("a", "b", "c");
    fakeLayout(container, [100, 100, 100]);
    press("j");
    press("j");
    expect(current(container)).toBe("c");
    press("g");
    // Ctrl+h to the editor (GTK takes the chord), then back by a click, much later.
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    act(() => root(container).focus());
    press("g");
    expect(current(container)).toBe("c"); // not gg: the cursor did not jump to "a"
  });
});

describe("App handoff to a terminal", () => {
  function conversation(overrides: Partial<AgentUiState> = {}, hello: Hello = HELLO) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...hello });
    dispatchLiveTab(snapshotState(overrides), 1);
    return rendered;
  }

  it("says why in visible text before the first turn -- no confirm dialog, no button either", () => {
    const { container } = conversation({ providerSessionId: null });
    openHandoffConfirm(container);
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("first turn");
    expect(container.querySelector(".handoff-confirm")).toBeNull();
  });

  it("asks Rust for the handoff only on the confirming click, never on opening it", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    openHandoffConfirm(container);
    expect(container.querySelector(".handoff-confirm")).not.toBeNull();
    expect(lastOfType("handoff_to_terminal")).toBeUndefined();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    const posted = lastOfType("handoff_to_terminal")!;
    expect(typeof posted.request_id).toBe("string");
    // Nothing about WHICH session beyond the tab: Rust reads the conversation itself from canonical
    // state, and a second source for it is how a panel eventually prints a command resuming some
    // other conversation.
    expect(Object.keys(posted).sort()).toEqual(["request_id", "tab", "type"]);
  });

  /* The envelope arrives only after the real close has finished, so by the time this renders the
     conversation genuinely is over -- which is why the transcript goes with it rather than being
     left on screen looking live. */
  it("replaces the conversation with the command once the session really has been closed", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2", transcript: [{ seq: 1, text: "earlier reply" }] });
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    // Rust's own tab, once really closed, returns to NotStarted (ruling 13) -- that `tabs` envelope
    // is what actually drops this render back to the empty tab.
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    expect(container.querySelector("pre.handoff-command")!.textContent).toBe(
      "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
    );
    // The sign column this row adopted: nothing previously asserted it carries `→`.
    expect(container.querySelector(".row-handoff")!.getAttribute("data-sign")).toBe("→");
    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    expect(container.textContent).not.toContain("earlier reply");
    // A new conversation is still startable; only this one moved.
    expect(container.querySelector(".empty-tab")).not.toBeNull();
  });

  /* The session was just given to a terminal with no lock held. Continuing to offer it here is the
     exact concurrency the card above it warns about, one click away. */
  it("stops offering to continue the session it just handed over", () => {
    const resumableHello: Hello = {
      ...HELLO,
      backend: "sidecar",
      resumableSessions: [{ provider: "claude", providerSessionId: "1857dcd5-973b-46a2", createdAt: "", updatedAt: "" }],
    };
    // Nothing to assert before the handoff: the conversation is on screen, so the start screen and
    // its resume offer are not rendered at all. The control for this test is its sibling below,
    // which reaches the same start screen by the same route and DOES still see the offer.
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" }, resumableHello);
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    expect(buttonLabelled(container, "Continue previous session")).toBeUndefined();
  });

  /* A start that fails must not take the command with it. On the legacy backend the id in that card
     is the last reference to the conversation anywhere in the system — Rust keeps its copy until a
     session is genuinely installed, and this is the frontend half of the same rule. */
  it("keeps the command on screen when the next session fails to start", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "try again" } });
    fireEvent.keyDown(box, { key: "Enter" });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: "claude is not on PATH" }] });
    expect(container.querySelector("pre.handoff-command")!.textContent).toContain("--resume 1857dcd5-973b-46a2");
  });

  /* Review round 1, item 2: `TerminalHandoff.tsx`'s "Press y to copy." promise was broken -- this
     card only ever renders on the start screen (`sessionStarted === false`), a render branch with
     no key handling of its own before this fix, and `resolveKey` gates `y` on nothing while the
     handoff card is not a `TimelineItem` a cursor could ever sit on anyway. `y` here goes through
     `App.tsx`'s own `handleStartScreenKeyDown`, a separate small handler rather than a branch in
     the conversation's `onKeyDown` -- the two never run at once, since `handoff` is always cleared
     by the time a real `snapshot` flips `sessionStarted` back to true. */
  it("copies the handoff command on y, once the session has been closed for it", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    const clipboard = stubClipboard();
    // F3's own composer autofocuses first; blur it (as a click elsewhere would) to reach the
    // handoff card's own promise, "Press y to copy.", the same way `y` in the composer would
    // otherwise just type the letter.
    fireEvent.blur(container.querySelector("textarea")!);
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledWith("cd /home/user/project && claude --resume 1857dcd5-973b-46a2");
  });

  /* ...and a session that really does start replaces it. The card is only ever drawn on the start
     screen, so the way this becomes visible is the round trip: hand off, start a session that runs,
     then have THAT session die — the start screen must show the new session's error, not the old
     conversation's command. */
  it("clears the command once a real session is running, so a later failure does not resurrect it", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    expect(container.querySelector("pre.handoff-command")).not.toBeNull();

    dispatchLiveTab(snapshotState(), 9);
    dispatch({ kind: "error", tab: 1, message: "the second session died" });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: "the second session died" }] });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    expect(container.querySelector("pre.handoff-command")).toBeNull();
  });

  /** v1 polish item 6: a resumed session whose sidecar stops before any new turn keeps its
   *  transcript on screen, as a lost session naming the sidecar, across a switch away and back;
   *  `r` (the tab no longer failed) lets it go. */
  it("keeps a conversation on screen when its sidecar stops, and says so", () => {
    const inner =
      "the connection to the provider ended before this session did, so anything after this point never arrived and the reply above may be incomplete (the provider closed the event stream)";
    const message = `the session ended before it started (${inner}). If you were continuing a previous conversation, it most likely no longer exists -- start a new session instead.`;
    const { container } = conversation({ transcript: [{ seq: 1, text: "restored reply" }] });
    dispatch({ kind: "error", tab: 1, message });
    const failed = { ...LIVE_TAB, state: "failed", failure: message } as const;
    const other = { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" } as const;
    dispatch({ kind: "tabs", active: 1, tabs: [failed, other] });
    const shown = () => container.textContent ?? "";
    expect(container.querySelector(".empty-tab")).toBeNull();
    expect(shown()).toContain("restored reply");
    expect(shown()).toContain("The agent sidecar stopped.");
    expect(shown()).not.toContain("most likely no longer exists");
    expect(container.querySelector(".fatal-error")).toBeNull();
    dispatch({ kind: "tabs", active: 2, tabs: [failed, other] });
    expect(shown()).not.toContain("restored reply");
    dispatch({ kind: "tabs", active: 1, tabs: [failed, other] });
    expect(shown()).toContain("restored reply");
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }, other] });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    expect(shown()).not.toContain("restored reply");
  });

  it("still fails the ordinary way when nothing was said before the sidecar stopped", () => {
    const { container } = conversation({});
    const message = "the connection to the provider ended before this session did (x)";
    dispatch({ kind: "error", tab: 1, message });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: message }] });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    expect(container.textContent).toContain("The agent sidecar stopped.");
  });

  /* A stored session that is NOT the one handed over is untouched -- suppressing every offer would
     hide a conversation nobody gave away. */
  it("leaves an unrelated stored session on offer", () => {
    const resumableHello: Hello = {
      ...HELLO,
      backend: "sidecar",
      resumableSessions: [{ provider: "claude", providerSessionId: "some-other-session", createdAt: "", updatedAt: "" }],
    };
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" }, resumableHello);
    dispatch({
      kind: "handoff", tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    // Panel round 2 (plan Task 12) replaced the empty tab's one-row-per-record picker with the
    // dashboard's single "Resume last" item (`dashItems`, spec §7) -- so the assertion this test
    // exists for is now that the item resumes the unrelated session's own id, not the handed-off
    // one, rather than that a row bearing its id text survives on screen (the dashboard shows no
    // record's id at all; `session()`'s title is what would show, and this record has none).
    const items = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="dash"]'));
    const resumeItem = items.find((el) => el.textContent?.includes("Resume last"));
    expect(resumeItem).toBeDefined();
    fireEvent.click(resumeItem!);
    expect(lastOfType("resume")).toMatchObject({ provider_session_id: "some-other-session" });
  });
});

/* The close window is not instantaneous — on the sidecar path `AgentBackend::shutdown` is a 10s
   unary RPC plus kill escalation, on legacy ~0.8s of grace periods — and for its whole length Rust
   holds no session and refuses every command. The frontend has to reflect that. */
describe("App while a handoff is closing the conversation", () => {
  /** Ends in BROWSE, not INPUT: `openHandoffConfirm` opens the detail popover on its way to the
   *  trailing row, and `tab_detail` forces BROWSE the same way `prefix i` does (pre-existing,
   *  unrelated to this task). A test that needs the composer back calls `enterInputMode` again. */
  function closing() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }), 1);
    enterInputMode(rendered.container);
    openHandoffConfirm(rendered.container);
    return rendered;
  }

  /* The outcome that is not acceptable is the message disappearing with no trace. It is still in
     the box, nothing was sent, and the box says why it stopped accepting input.
     Panel round 2 (plan Task 10): typed BEFORE `closing()` now, not after -- `tab_detail` (the
     detail popover `openHandoffConfirm` opens on its way to the trailing row) forces BROWSE the
     same way `prefix i` does, so INPUT is gone by the time `closing()` returns. The composer's own
     `text` state survives that, unmounted branch and all (React keeps a component's state across
     its own conditional rendering), so `enterInputMode` afterwards is what proves it -- the same
     control, not a fresh one. */
  it("does not swallow a message typed before the conversation started closing", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }), 1);
    enterInputMode(container);
    fireEvent.change(container.querySelector("textarea")!, { target: { value: "a long prompt worth not losing" } });
    openHandoffConfirm(container);
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    enterInputMode(container);

    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
    expect(lastOfType("send_message")).toBeUndefined();
    expect(container.querySelector("textarea")!.value).toBe("a long prompt worth not losing");
    expect(container.querySelector("textarea")!.disabled).toBe(true);
    expect(container.querySelector(".composer-closing")!.textContent).toContain("not");
  });

  /* Rust refuses a second handoff ("this conversation is already being handed off"), and that
     refusal only ever reached a console.warn. The control is not offered again in the first place. */
  it("stops offering the handoff control while one is already in flight, with the reason visible", () => {
    const { container } = closing();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    expect(container.querySelector(".handoff-confirm")).toBeNull();
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("being closed");
  });

  /* The closing state belongs to the tab being handed off (whole-branch review): switching to
     another live tab while tab 1's close is still running must leave that tab's composer usable,
     and tab 1's own `command_result` -- answered while tab 2 is on screen -- ends tab 1's state. */
  it("keeps the closing state on the tab being handed off, not on whichever tab is shown", () => {
    const { container } = closing();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    const requestId = lastOfType("handoff_to_terminal")!.request_id;
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    enterInputMode(container);
    expect(container.querySelector("textarea")!.disabled).toBe(false);
    expect(container.querySelector(".composer-closing")).toBeNull();

    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }) });
    enterInputMode(container);
    expect(container.querySelector("textarea")!.disabled).toBe(true);

    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "command_result", requestId, ok: true });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }) });
    enterInputMode(container);
    expect(container.querySelector("textarea")!.disabled).toBe(false);
  });

  /* A stale-view refusal (a turn started between the render and the click) leaves the session
     completely untouched on the Rust side, so the panel has to come back to life here too. */
  it("comes back to life, saying why, when Rust refuses the handoff", () => {
    const { container } = closing();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    const requestId = lastOfType("handoff_to_terminal")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "A turn is still running." });
    // `closing()` ends in BROWSE (`openHandoffConfirm`'s own doc comment); `enterInputMode` here
    // is what checks the composer itself, not a side effect of the refusal.
    enterInputMode(container);
    expect(container.querySelector("textarea")!.disabled).toBe(false);
    expect(container.querySelector(".command-notice")!.textContent).toContain("A turn is still running.");
  });
});

/* Every Rust refusal carries a real human-readable reason and none of them used to be shown. The
   send case is the one that also loses data, because the composer clears optimistically. */
describe("App refused commands", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    enterInputMode(rendered.container);
    return rendered;
  }

  it("puts a refused message back in the box and says why", () => {
    const { container } = startedApp();
    fireEvent.change(container.querySelector("textarea")!, { target: { value: "please keep me" } });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
    const requestId = lastOfType("send_message")!.request_id;
    // Optimistically cleared, which is only acceptable because of what happens next.
    expect(container.querySelector("textarea")!.value).toBe("");

    dispatch({ kind: "command_result", requestId, ok: false, error: "no active session" });
    expect(container.querySelector("textarea")!.value).toBe("please keep me");
    expect(container.querySelector(".command-notice")!.textContent).toContain("no active session");
  });

  it("restores the same text a second time when it is refused again", () => {
    const { container } = startedApp();
    for (const _ of [0, 1]) {
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "same text twice" } });
      fireEvent.keyDown(container.querySelector("textarea")!, { key: "Enter" });
      const requestId = lastOfType("send_message")!.request_id;
      dispatch({ kind: "command_result", requestId, ok: false, error: "no active session" });
      expect(container.querySelector("textarea")!.value).toBe("same text twice");
      // Cleared by hand so the second round genuinely has to restore it again.
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "" } });
    }
  });

  /* P2-A3 (v1 audit), ruling R1: the composer is unchanged since the optimistic clear (nothing was
     typed while the reply was in flight), so the refused text is restored alone -- and it is
     mirrored back to Rust in the same `draft` envelope ordinary typing sends, which is what this
     panel needs before a later tab switch can bring it back (`shell::tabs::set_draft` keeps
     whatever it is told regardless of how the send that produced it was refused). */
  it("mirrors a restored refused send back to Rust when the box was left untouched", () => {
    vi.useFakeTimers();
    try {
      const { container } = startedApp();
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "please keep me" } });
      fireEvent.keyDown(box, { key: "Enter" });
      const requestId = lastOfType("send_message")!.request_id;
      dispatch({ kind: "command_result", requestId, ok: false, error: "no active session" });
      expect(box.value).toBe("please keep me");
      expect(lastOfType("draft")).toBeUndefined();
      act(() => vi.advanceTimersByTime(300));
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "please keep me" });
    } finally {
      vi.useRealTimers();
    }
  });

  /* R1's "keep both": text typed into the box while the send was still in flight must not be
     destroyed by the refused text coming back -- the refused text goes BEFORE what was typed,
     separated by a newline, and the combined text is what reaches Rust. */
  it("keeps text typed while a send was refused, with the refused text placed before it", () => {
    vi.useFakeTimers();
    try {
      const { container } = startedApp();
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "first prompt" } });
      fireEvent.keyDown(box, { key: "Enter" });
      const requestId = lastOfType("send_message")!.request_id;
      // Typed after the optimistic clear, before Rust's refusal arrives.
      fireEvent.change(box, { target: { value: "second unsent draft" } });
      dispatch({ kind: "command_result", requestId, ok: false, error: "transport refused" });
      expect(box.value).toBe("first prompt\nsecond unsent draft");
      act(() => vi.advanceTimersByTime(300));
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "first prompt\nsecond unsent draft" });
    } finally {
      vi.useRealTimers();
    }
  });

  /* The queue_message path (a refusal while a turn is showing as running) goes through the exact
     same restore code as send_message -- this is the probe's own second case, unchanged-since-send
     on that path. */
  it("mirrors a restored refused queue_message back to Rust", () => {
    vi.useFakeTimers();
    try {
      const { container } = runningApp();
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "unqueued text" } });
      fireEvent.keyDown(box, { key: "Enter" });
      const requestId = lastOfType("queue_message")!.request_id;
      dispatch({ kind: "command_result", requestId, ok: false, error: "session ended" });
      expect(box.value).toBe("unqueued text");
      act(() => vi.advanceTimersByTime(300));
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "unqueued text" });
    } finally {
      vi.useRealTimers();
    }
  });

  /* The point of mirroring the recovered text to Rust at all: a later tab switch brings it back,
     the same way any other draft does (`"takes each tab's draft from Rust, never from the tab it
     left"` in the tabs describe block, exercised here starting from a refusal instead of plain
     typing). Switches INSIDE the refusal's own 300ms debounce, before advancing any timer -- fix
     round 1 (v1 audit, codex): the previous version of this test advanced the debounce first, so it
     only ever pinned a hand-dispatched `draft` echo (already covered by the tabs block's "takes
     each tab's draft from Rust" test), never the actual race where `flushDraft` in the `tabs`
     handler (ruling 6) is what has to deliver the recovered text on a switch nobody waited out. */
  it("shows the recovered text again after switching tabs and back", () => {
    vi.useFakeTimers();
    try {
      const { container } = startedApp();
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "first prompt" } });
      fireEvent.keyDown(box, { key: "Enter" });
      const requestId = lastOfType("send_message")!.request_id;
      dispatch({ kind: "command_result", requestId, ok: false, error: "transport refused" });
      expect(box.value).toBe("first prompt");
      // Not yet flushed -- the refusal's own debounce has not fired, and nothing has been posted.
      expect(lastOfType("draft")).toBeUndefined();

      const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
      dispatch({ kind: "tabs", active: 2, tabs: two });
      // The switch itself flushed the still-pending mirror -- no timer was advanced.
      const mirrored = lastOfType("draft");
      expect(mirrored).toMatchObject({ tab: 1, text: "first prompt" });
      dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
      dispatch({ kind: "tabs", active: 1, tabs: two });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
      // Rust echoing back exactly what the switch mirrored to it -- the contract this task closes.
      dispatch({ kind: "draft", tab: 1, text: mirrored!.text as string });
      enterInputMode(container);
      expect(container.querySelector("textarea")!.value).toBe("first prompt");
    } finally {
      vi.useRealTimers();
    }
  });

  /* [important, codex] fix round 1: `reset_tab` (the `r` restart on an ended tab) used to read
     Rust's own, possibly stale `tab.draft` (`TabSet::reset` joins `tab.queue` and `tab.draft` --
     see core/src/tab_set.rs) without first delivering whatever recovery mirror was still sitting in
     the 300ms debounce this panel just armed. Pressing `r` inside that window sent `reset_tab`
     first, and the `draft` echo that came back (Rust's stale copy) both wiped the composer AND
     nulled `pendingDraftRef`, so the debounce's own later firing posted nothing -- the recovered
     text reached neither the box nor Rust. Fixed by flushing the pending mirror before `reset_tab`,
     the same way a real tab switch already does (ruling 6). */
  it("flushes a still-pending recovered draft before reset_tab, so a restart cannot erase it", () => {
    vi.useFakeTimers();
    try {
      const { container } = startedApp();
      const box = container.querySelector("textarea")!;
      fireEvent.change(box, { target: { value: "first prompt" } });
      fireEvent.keyDown(box, { key: "Enter" });
      const requestId = lastOfType("send_message")!.request_id;
      // Typed after the optimistic clear, before Rust's refusal arrives -- R1's "keep both".
      fireEvent.change(box, { target: { value: "second unsent draft" } });
      dispatch({ kind: "command_result", requestId, ok: false, error: "no active session" });
      expect(box.value).toBe("first prompt\nsecond unsent draft");
      // Not yet flushed -- the refusal's own `mirrorDraft` debounce has not fired.
      expect(lastOfType("draft")).toBeUndefined();

      // The same failure that refused the send also ended the tab, so `r` is on offer; pressed
      // inside the still-pending debounce window, before any timer fires on its own.
      dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events: [{ type: "session_closed", reason: "provider exited" }] });
      fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "r" });

      // The recovered text reached Rust before reset_tab, not after it, and not a stray empty one.
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "first prompt\nsecond unsent draft" });
      expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });
      expect(posted.indexOf(lastOfType("draft")!)).toBeLessThan(posted.indexOf(lastOfType("reset_tab")!));

      // The now-superseded timer firing later must not post a second, stale (or empty) draft.
      act(() => vi.advanceTimersByTime(300));
      expect(posted.filter((m) => m.type === "draft")).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  function runningApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 0);
    enterInputMode(rendered.container);
    return rendered;
  }

  /* Review finding (phase 3): a queue the panel believed could take a message (a turn still showing
     as running) but Rust refused -- the session ended, or a handoff is pending -- used to clear the
     box and keep the text nowhere, not even in history (`remember_prompts` runs only on `Ok`). */
  it("puts a refused queue_message back in the box", () => {
    const { container } = runningApp();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "queue me please" } });
    fireEvent.keyDown(box, { key: "Enter" });
    const requestId = lastOfType("queue_message")!.request_id;
    expect(container.querySelector("textarea")!.value).toBe("");

    dispatch({
      kind: "command_result",
      requestId,
      ok: false,
      error: "this tab's session has ended; press r to start over",
    });
    expect(container.querySelector("textarea")!.value).toBe("queue me please");
    expect(container.querySelector(".command-notice")!.textContent).toContain("session has ended");
  });

  /* `send_now`'s refusal can arrive AFTER its text was queued (the interrupt was refused, ruling 8:
     "the queue stays"), so putting it back would show it twice. Rust saves it to history before the
     result either way, which is what keeps a refused `send_now` recoverable. */
  it("does not put a refused send_now back in the box", () => {
    const { container } = runningApp();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "now please" } });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    const requestId = lastOfType("send_now")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "interrupt refused" });
    expect(container.querySelector("textarea")!.value).toBe("");
    expect(container.querySelector(".command-notice")!.textContent).toContain("interrupt refused");
  });

  // "does not double-report a failed start, which already has its own banner" was removed here
  // (session tabs Task 9): it guarded `record?.kind === "start"`, a distinction that existed only
  // because `start_session` was its own wire message with its own `command_result`. There is no
  // such message any more (ruling 4) -- a fresh tab's first `send_message` IS the start, and a
  // refusal of it is refused the same way any other send is, with no separate suppression rule
  // specified for it by the plan.
});

describe("App: a live session's ending classified (spec §10.2, P11)", () => {
  function ended(type: "session_unavailable" | "session_closed", reason: string) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events: [{ type, reason }] });
    return rendered;
  }

  it("a lost session whose reason is a login failure gets the headline above the unchanged raw text", () => {
    const reason = "provider failed: Invalid API key · Please run /login";
    const { container } = ended("session_unavailable", reason);
    const row = container.querySelector(".row-error")!;
    expect(row.querySelector(".row-problem strong")!.textContent).toBe("Claude Code is not logged in.");
    expect(row.querySelector("pre")!.textContent).toBe(reason);
  });

  it("a closed session is classified the same way", () => {
    const { container } = ended("session_closed", "Please run /login");
    expect(container.querySelector(".row-ended .row-problem strong")!.textContent).toBe("Claude Code is not logged in.");
  });

  it("an unrecognised reason draws exactly what it drew before", () => {
    const { container } = ended("session_unavailable", "provider exited");
    expect(container.querySelector(".row-problem")).toBeNull();
    expect(container.querySelector(".row-error pre")!.textContent).toBe("provider exited");
  });
});

describe("App fatal errors", () => {
  it("shows the whole error text on the empty tab, with the dead session's transcript gone", () => {
    // `errorBanner` (`.fatal-error`) is deliberately kept out of the empty-tab render (Step 12): a
    // failed tab shows its own reason inline instead (ruling 14), which is what this now asserts on.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 0, text: "gone" }] }), 0);
    dispatch({ kind: "error", tab: 1, message: "sidecar handshake failed\nclaude CLI 2.1.272 is untested" });
    // Ruling 14: a fatal command marks the tab Failed, and it is that `tabs` envelope -- not the
    // `error` alone -- that actually drops this render back to the empty tab.
    dispatch({
      kind: "tabs",
      active: 1,
      tabs: [{ ...LIVE_TAB, state: "failed", failure: "sidecar handshake failed\nclaude CLI 2.1.272 is untested" }],
    });
    expect(container.querySelector(".fatal-error")).toBeNull();
    const failedRow = container.querySelector(".empty-tab .row-error")!;
    expect(failedRow.textContent).toContain("claude CLI 2.1.272 is untested");
    // The dead session's transcript is gone with it, rather than left on screen looking live.
    expect(container.textContent).not.toContain("gone");
  });

  // "asks for a fresh hello when a fatal error drops it back to the start screen" and "re-asks once
  // per error rather than compounding" were removed here (session tabs Task 9, ruling 17): `error`
  // no longer re-requests `hello` itself. Rust re-sends `hello`, recomputed, whenever the set of
  // open provider sessions changes -- a tab failing is exactly such a change -- so there is no
  // client-side re-ask left to test; asserting "no extra `ready` is posted" would just restate that
  // `error`'s handler does not call `requestHello()`, which is visible in `App.tsx` itself.

  // "can be dismissed without resurrecting the session" was removed here too: it exercised
  // `errorBanner`'s own Dismiss button, over a session that was never started at all -- a scenario
  // that never reaches the conversation branch `errorBanner` renders in any more, since a bare
  // `error` before any tab exists goes straight to the empty tab. A failed tab's inline reason has
  // no separate dismissal; `r` (ruling 12) is what starts over.
});

/* The `?` keymap overlay (Task 3, spec 2026-09-19-which-key-design.md §3), wired end to end through
   `App.tsx`'s `onKeyDown` rather than `KeymapOverlay` in isolation -- `KeymapOverlay.test.tsx`
   already covers the tables/titles/backdrop-click, so what belongs here is the part only the wiring
   can prove: that opening it comes from the real key table, and that once it is open it truly owns
   every key ahead of `resolveKey` -- a pending permission and the row cursor included. */
describe("App: the ? keymap overlay (spec 2026-09-19-which-key-design.md §3)", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(overrides), 0);
    dispatch({
      kind: "keymap",
      prefix: "Ctrl+b",
      window: [{ keys: "F11", what: "Fullscreen" }],
      prefixKeys: [{ keys: "Ctrl+b f", what: "HINT: jump anywhere in the window" }],
      panel: EMPTY_PANEL_TABLE,
      newTabChord: "Ctrl+b c",
    });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string, over: Partial<{ shiftKey: boolean }> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...over });
  const overlay = (c: HTMLElement) => c.querySelector<HTMLElement>(".keymap-overlay");

  it("opens on ? and shows all six groups, in the spec's order", () => {
    const { container } = started();
    act(() => root(container).focus());
    expect(overlay(container)).toBeNull();
    // `?` almost always arrives as Shift+/ -- the same reason `resolveKey`'s own test checks it
    // both ways (`keymap.test.ts`).
    press("?", { shiftKey: true });
    const el = overlay(container);
    expect(el).not.toBeNull();
    const titles = Array.from(el!.querySelectorAll("h2")).map((h) => h.textContent);
    // "Selecting" (visual-mode spec, D2) sits right after "This panel"; "Leader and tab keys"
    // (panel round 2 plan, Task 8) after that, then "Typing"; "Slash commands" (spec §9.2, P10)
    // sits between "Typing" and "Anywhere".
    expect(titles).toEqual([
      "This panel",
      "Selecting (v)",
      "Leader and tab keys",
      "Typing",
      "Slash commands",
      "Anywhere in the window",
      "After Ctrl+b",
    ]);
  });

  it("shows the rows shell sent in its keymap envelope", () => {
    const { container } = started();
    act(() => root(container).focus());
    press("?", { shiftKey: true });
    expect(overlay(container)!.textContent).toContain("Ctrl+b f");
  });

  it("opens on shell's open_keymap, in BROWSE, even from INPUT", () => {
    const { container } = started();
    enterInputMode(container);
    act(() => dispatch({ kind: "open_keymap" }));
    expect(overlay(container)).not.toBeNull();
    expect(container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode).toBe("browse");
  });

  /** Panel round 2 (spec §7) gave the empty tab a `? Keys` item and `?` on an empty draft, so the
   *  start screen draws the overlay now. Before this GUI-pass fix (2026-09-26, r2-gui) it drew
   *  none, and the dashboard's `?` armed one anyway: nothing showed, and the overlay popped up over
   *  the conversation the moment a session started -- the failure ruling 11's guard was written for. */
  it("opens on the start screen too, and closes there, so it never pops up once a session starts", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    act(() => dispatch({ kind: "open_keymap" }));
    expect(overlay(container)).not.toBeNull();
    fireEvent.keyDown(container.querySelector(".agent-ui-root")!, { key: "q" });
    expect(overlay(container)).toBeNull();
    dispatchLiveTab(snapshotState(), 0);
    expect(overlay(container)).toBeNull();
  });

  it("opens from the empty tab's own `?` item, and a key typed under it reaches nothing else", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    const keys = Array.from(container.querySelectorAll<HTMLElement>(".dash-item")).find((el) => el.textContent?.includes("Keys"))!;
    fireEvent.click(keys);
    expect(overlay(container)).not.toBeNull();
    // `w` would open the session chooser from the dashboard (v1: `m` no longer does anything at
    // all, on the dashboard or off it); under the overlay it is swallowed.
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "w" });
    expect(lastOfType("tab_verb")).toBeUndefined();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "Escape" });
    expect(overlay(container)).toBeNull();
  });

  it.each(["?", "Escape", "q"])("closes on %s", (key) => {
    const { container } = started();
    act(() => root(container).focus());
    press("?", { shiftKey: true });
    expect(overlay(container)).not.toBeNull();
    press(key);
    expect(overlay(container)).toBeNull();
  });

  it("swallows a and d while open -- a pending card underneath is not answered", () => {
    const { container } = started();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: {} },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    act(() => root(container).focus());
    press("?", { shiftKey: true });
    press("a");
    press("d");
    expect(lastOfType("permission_response")).toBeUndefined();
  });

  it("swallows j -- the row cursor underneath does not move", () => {
    const { container } = started();
    events({ type: "user_prompt_submitted", text: "one" }, { type: "user_prompt_submitted", text: "two" });
    act(() => root(container).focus());
    const before = container.querySelector(".row-current")?.textContent;
    press("?", { shiftKey: true });
    press("j");
    expect(container.querySelector(".row-current")?.textContent).toBe(before);
  });

  it("closes when a global HINT starts elsewhere in the window (hint_collect)", () => {
    const { container } = started();
    act(() => root(container).focus());
    press("?", { shiftKey: true });
    expect(overlay(container)).not.toBeNull();
    dispatch({ kind: "hint_collect", sessionId: 1 });
    expect(overlay(container)).toBeNull();
  });
});

/* R9 (idiom matrix E10; kbux pass 2026-09-29, row P15): `Ctrl+[` is Esc, as in vim, nvim and every
   terminal. WebKitGTK 2.52.6 delivers it as `key "["`, `code BracketLeft`, `ctrlKey`, and nothing
   here read that as Esc. One document-capture listener (`./ctrlBracket`, installed by `App`'s first
   effect) re-dispatches it as a plain Escape keydown on the same target, so every place below is a
   place the panel already reads Escape -- each test drives the real `App` with `Ctrl+[` where its
   Escape tests drive it with `Escape`. The CARET/VISUAL half is in "BROWSE visual mode" further down. */
describe("Ctrl+[ is Esc everywhere in the panel (R9, kbux P15)", () => {
  const CTRL_BRACKET = { key: "[", code: "BracketLeft", ctrlKey: true };
  const CHOOSER = {
    kind: "chooser",
    open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }],
    records: [],
  };
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const modeOf = (c: HTMLElement) => c.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode;

  function started() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    // The band names the mode only while this pane holds the keys (v1 polish F24).
    dispatch({ kind: "pane_focus", focused: true });
    return rendered;
  }

  it("INPUT, then Ctrl+[, lands in BROWSE, as Esc does", () => {
    const { container } = started();
    enterInputMode(container);
    const textarea = container.querySelector("textarea")!;
    expect(textarea).not.toBeNull();
    expect(modeOf(container)).toBe("input");
    expect(fireEvent.keyDown(textarea, CTRL_BRACKET), "the Ctrl+[ itself is claimed").toBe(false);
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-browse-hint")).not.toBeNull();
    expect(modeOf(container)).toBe("browse");
  });

  it("closes the chooser", () => {
    const { container } = started();
    dispatch(CHOOSER);
    expect(container.querySelector(".chooser")).not.toBeNull();
    fireEvent.keyDown(container.querySelector(".chooser")!, CTRL_BRACKET);
    expect(container.querySelector(".chooser")).toBeNull();
  });

  it("closes the chooser's filter box, then the chooser, one Esc each", () => {
    const { container } = started();
    dispatch(CHOOSER);
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(filter).not.toBeNull();
    fireEvent.keyDown(filter, CTRL_BRACKET);
    expect(container.querySelector(".chooser-filter"), "the first leaves the filter").toBeNull();
    expect(container.querySelector(".chooser"), "and only the filter").not.toBeNull();
    fireEvent.keyDown(container.querySelector(".chooser")!, CTRL_BRACKET);
    expect(container.querySelector(".chooser")).toBeNull();
  });

  it("closes the ? overlay", () => {
    const { container } = started();
    act(() => root(container).focus());
    fireEvent.keyDown(root(container), { key: "?", shiftKey: true });
    const overlay = container.querySelector<HTMLElement>(".keymap-overlay");
    expect(overlay).not.toBeNull();
    fireEvent.keyDown(overlay!, CTRL_BRACKET);
    expect(container.querySelector(".keymap-overlay")).toBeNull();
  });

  it("closes the ? overlay on the empty tab's start screen too", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    act(() => dispatch({ kind: "open_keymap" }));
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, CTRL_BRACKET);
    expect(container.querySelector(".keymap-overlay")).toBeNull();
  });

  it("closes the / prompt", () => {
    const { container } = started();
    fireEvent.keyDown(root(container), { key: "/" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input");
    expect(input).not.toBeNull();
    fireEvent.keyDown(input!, CTRL_BRACKET);
    expect(container.querySelector(".search-bar")).toBeNull();
  });

  it("a composing Ctrl+[ does nothing: an input method's own key, as Esc is", () => {
    const { container } = started();
    enterInputMode(container);
    const textarea = container.querySelector("textarea")!;
    for (const composing of [{ isComposing: true }, { keyCode: 229 }]) {
      expect(fireEvent.keyDown(textarea, { ...CTRL_BRACKET, ...composing }), "left to the input method").toBe(true);
      expect(container.querySelector("textarea"), JSON.stringify(composing)).not.toBeNull();
      expect(modeOf(container)).toBe("input");
    }
    // The same over an overlay: the ? overlay closes on a real Ctrl+[, not on a composing one.
    fireEvent.keyDown(textarea, { key: "Escape" });
    fireEvent.keyDown(root(container), { key: "?", shiftKey: true });
    const overlay = container.querySelector<HTMLElement>(".keymap-overlay")!;
    fireEvent.keyDown(overlay, { ...CTRL_BRACKET, isComposing: true });
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
    fireEvent.keyDown(overlay, CTRL_BRACKET);
    expect(container.querySelector(".keymap-overlay")).toBeNull();
  });

  it("only a plain Ctrl+[ leaves INPUT: with Shift, Alt or Meta held, or with no Ctrl, it does not", () => {
    const { container } = started();
    enterInputMode(container);
    const textarea = container.querySelector("textarea")!;
    for (const init of [
      { key: "{", ctrlKey: true, shiftKey: true },
      { ...CTRL_BRACKET, altKey: true },
      { ...CTRL_BRACKET, metaKey: true },
      { key: "[" },
    ]) {
      fireEvent.keyDown(textarea, init);
      expect(container.querySelector("textarea"), JSON.stringify(init)).not.toBeNull();
    }
  });

  it("stops claiming Ctrl+[ once the panel unmounts", () => {
    const { unmount } = started();
    expect(fireEvent.keyDown(document.body, CTRL_BRACKET), "claimed while mounted").toBe(false);
    unmount();
    expect(fireEvent.keyDown(document.body, CTRL_BRACKET), "not claimed once gone").toBe(true);
  });
});

describe("V2/panel round 2: the activity line, the bottom band, and the detail popover (Task 10)", () => {
  it("puts the activity line above the composer, then the band -- and none of the components it replaced", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1", model: "claude-sonnet-5" }));
    const order = [".activity-line", ".composer", ".status-band"].map((s) => container.querySelector(s));
    expect(order.every((el) => el !== null)).toBe(true);
    for (let i = 1; i < order.length; i++) {
      expect(order[i - 1]!.compareDocumentPosition(order[i]!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    }
    expect(container.querySelector(".winbar")).toBeNull();
    // Panel round 2 (plan Task 10): `StatusRow`, `Footer`, `NewPill`, `ContextLine` and the
    // per-row which-key strip are all gone, replaced by the one band above.
    expect(container.querySelector(".status-row")).toBeNull();
    expect(container.querySelector(".panel-footer")).toBeNull();
    expect(container.querySelector(".new-pill")).toBeNull();
    expect(container.querySelector(".context-line")).toBeNull();
    expect(container.querySelector(".which-key")).toBeNull();
  });

  it("opens the detail popover from a click on the band and closes it with Esc", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState());
    fireEvent.click(container.querySelector(".band-open")!);
    expect(lastOfType("open_detail")).toMatchObject({ tab: 1 });
    dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "account", value: "work" }, { label: "cwd", value: "/p" }] });
    expect(container.querySelector(".detail-popover")!.textContent).toContain("work");
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "Escape" });
    expect(container.querySelector(".detail-popover")).toBeNull();
  });

  /** Panel round 2 (plan Task 10; spec §5.4): the detail popover's trailing row, not `Enter` on
   *  nothing, is what opens `ContinueInTerminal`'s confirmation now -- `App.test.tsx`'s own bullet
   *  for this task. */
  it("the popover's trailing row opens ContinueInTerminal's confirmation, above the composer", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }));
    fireEvent.click(container.querySelector(".band-open")!);
    dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "account", value: "work" }] });
    expect(container.querySelector(".detail-popover")).not.toBeNull();
    const handoffRow = Array.from(container.querySelectorAll(".detail-popover tr")).find((tr) =>
      tr.textContent?.includes("Continue in a terminal"),
    )!;
    fireEvent.click(handoffRow);
    expect(container.querySelector(".detail-popover")).toBeNull();
    const confirm = container.querySelector(".handoff-confirm")!;
    expect(confirm).not.toBeNull();
    expect(confirm.compareDocumentPosition(container.querySelector(".composer")!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});

/* Change B of the 2026-09-24 fix: `MessageList` ends following on what the user did, and for the
   panel's own scroll keys that is this announcement, made on the list before the key scrolls it.
   Without it, `k` would still end following only through the direction of its scroll -- the very
   signal an engine-originated drop once faked (see `./follow.ts`). */
describe("the panel's own scroll keys announce themselves to the message list", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        transcript: [
          { seq: 0, text: "one" },
          { seq: 1, text: "two" },
          { seq: 2, text: "three" },
        ],
      }),
      3,
    );
    return rendered;
  }

  it("says up for k, Ctrl+u and gg, and down for j, Ctrl+d and G", () => {
    const { container } = startedApp();
    const list = container.querySelector(".message-list")!;
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    const root = container.querySelector(".agent-ui-conversation")!;
    for (const key of [
      { key: "j" },
      { key: "k" },
      { key: "d", ctrlKey: true },
      { key: "u", ctrlKey: true },
      { key: "G", shiftKey: true },
      { key: "g" }, // the first half of `gg` moves nothing, and says nothing
      { key: "g" },
      // v1 trial item 5: `Ctrl+e` follows the same "down" rule as `j`/`Ctrl+d`/`G` (the scroll
      // decides whether following re-arms, once it actually reaches the bottom); `Ctrl+y` follows
      // the same "up" rule as `k`/`Ctrl+u`/`gg` (following stops outright, before the scroll).
      { key: "e", ctrlKey: true },
      { key: "y", ctrlKey: true },
    ]) {
      fireEvent.keyDown(root, key);
    }
    expect(seen).toEqual(["down", "up", "down", "up", "down", "up", "down", "up"]);
  });

  /* Task 3 (v1 picks, R2): `{N}G` / `{N}gg` go to row N, so the direction they announce is where that
     row sits against the cursor -- not the end a bare `G`/`gg` names whatever the cursor is (above). */
  it("says which way a counted jump goes: up to an earlier row, down to a later one", () => {
    const { container } = startedApp();
    const list = container.querySelector(".message-list")!;
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    const root = container.querySelector(".agent-ui-conversation")!;
    // The snapshot lands the cursor on the last of the three rows.
    for (const key of [
      { key: "1" }, { key: "G", shiftKey: true }, // row 1, from row 3: up
      { key: "3" }, { key: "G", shiftKey: true }, // row 3, from row 1: down
      { key: "2" }, { key: "g" }, { key: "g" }, // row 2, from row 3: up (the first g says nothing)
    ]) {
      fireEvent.keyDown(root, key);
    }
    expect(seen).toEqual(["up", "down", "up"]);
  });

  it("says nothing for a key that cannot move the conversation", () => {
    const { container } = startedApp();
    const list = container.querySelector(".message-list")!;
    const seen: unknown[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "?" }); // opens the keymap overlay
    fireEvent.keyDown(root, { key: "j" }); // ...which scrolls the overlay, not the list
    fireEvent.keyDown(root, { key: "Escape" });
    expect(seen).toEqual([]);
  });

  /* The phase-3 GUI pass (2026-09-25): a message sent while scrolled up landed the cursor on the new
     prompt but left the reply streaming below the view. A send the user makes resumes following; an
     Enter that only queues behind a running turn sends nothing yet, and says nothing. */
  it("resumes following on a send, on Ctrl+Enter over a running turn, and not on a queue", () => {
    const { container } = startedApp();
    const list = container.querySelector(".message-list")!;
    let resumed = 0;
    list.addEventListener(RESUME_FOLLOW_EVENT, () => resumed++);
    enterInputMode(container);
    const box = () => container.querySelector("textarea")!;
    fireEvent.change(box(), { target: { value: "send me" } });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(lastOfType("send_message")!.text).toBe("send me");
    expect(resumed).toBe(1);
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "turn_started", turn_id: "t1" }] });
    fireEvent.change(box(), { target: { value: "queue me" } });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(lastOfType("queue_message")!.text).toBe("queue me");
    expect(resumed).toBe(1);
    fireEvent.change(box(), { target: { value: "now" } });
    fireEvent.keyDown(box(), { key: "Enter", ctrlKey: true });
    expect(lastOfType("send_now")!.text).toBe("now");
    expect(resumed).toBe(2);
  });
});

describe("session tabs", () => {
  it("drops an envelope for a tab that is not active", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "tab one's reply" }] }));
    dispatch({ kind: "events", tab: 2, fromRevision: 1, throughRevision: 2, events: [{ type: "content_delta", turn_id: "t", kind: "text", text: "tab two's reply" }] });
    expect(container.textContent).toContain("tab one's reply");
    expect(container.textContent).not.toContain("tab two's reply");
  });

  it("names the active tab on every command", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 4, tabs: [{ ...LIVE_TAB, id: 4, state: "not_started" }] });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "hello" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(lastOfType("send_message")).toMatchObject({ tab: 4, text: "hello" });
  });

  it("a switch clears the previous tab's conversation before the new snapshot", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first tab" }] }));
    dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }] });
    expect(container.textContent).not.toContain("first tab");
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "second tab" }] }) });
    expect(container.textContent).toContain("second tab");
  });
});

describe("the tab bar", () => {
  const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 docs" }];

  it("appears with a second tab and goes with it (owner: 两个以上才显示)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState());
    expect(container.querySelector(".tab-bar")).toBeNull();
    dispatch({ kind: "tabs", active: 1, tabs: two });
    expect(container.querySelector(".tab-bar")).not.toBeNull();
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    expect(container.querySelector(".tab-bar")).toBeNull();
  });

  it("is the first nav stop: k from the first row reaches it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "only row" }] }) });
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "k" });
    expect(document.activeElement?.getAttribute("role")).toBe("tab");
  });

  it("keeps each tab's cursor across a switch", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const rows = snapshotState({ transcript: [1, 2, 3].map((seq) => ({ seq, text: `row ${seq}` })) });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: rows });
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "j" });
    const cursorText = () => container.querySelector(".row-current")?.textContent ?? "";
    expect(cursorText()).toContain("row 3");
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "other" }] }) });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: rows });
    expect(cursorText()).toContain("row 3");
  });

  it("mirrors the draft to Rust 300ms after the last change, and at once when the tab is left", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      const empty = [{ ...LIVE_TAB, state: "not_started" }, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
      dispatch({ kind: "tabs", active: 1, tabs: empty });
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "half" } });
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "half a thought" } });
      expect(lastOfType("draft")).toBeUndefined();
      act(() => vi.advanceTimersByTime(300));
      expect(posted.filter((m) => m.type === "draft")).toEqual([
        expect.objectContaining({ type: "draft", tab: 1, text: "half a thought" }),
      ]);
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "half a thought, more" } });
      dispatch({ kind: "tabs", active: 2, tabs: empty });
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "half a thought, more" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("takes each tab's draft from Rust, never from the tab it left", () => {
    // A switch lands BROWSE since the r2-gui pass (decision 4, onto an empty tab too), where the
    // draft is the stand-in's preview rather than a textarea's value.
    const draft = (c: HTMLElement) =>
      c.querySelector("textarea")?.value ?? c.querySelector(".composer-draft")?.textContent ?? "";
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const empty = [{ ...LIVE_TAB, state: "not_started" }, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
    dispatch({ kind: "tabs", active: 1, tabs: empty });
    fireEvent.change(container.querySelector("textarea")!, { target: { value: "tab one's words" } });
    dispatch({ kind: "tabs", active: 2, tabs: empty });
    dispatch({ kind: "draft", tab: 2, text: "" });
    expect(draft(container)).toBe("");
    dispatch({ kind: "draft", tab: 1, text: "stale, for a tab not on screen" });
    expect(draft(container), "dropped: not the active tab").toBe("");
    dispatch({ kind: "tabs", active: 1, tabs: empty });
    dispatch({ kind: "draft", tab: 1, text: "tab one's words" });
    expect(draft(container)).toBe("tab one's words");
  });

  it("prefix , renames inline and prefix & closes after y, any other key cancels", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "begin_rename", tab: 1, current: null });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    expect(container.querySelector(".tab-bar")).not.toBeNull();
    fireEvent.change(input, { target: { value: "docs" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(lastOfType("rename_tab")).toMatchObject({ tab: 1, name: "docs" });
    expect(container.querySelector(".tab-bar")).toBeNull();

    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({ kind: "confirm_close", tab: 1, lines: ['close 1 "docs"? (y/n)'] });
    fireEvent.keyDown(root, { key: "n" });
    expect(lastOfType("close_tab")).toBeUndefined();
    dispatch({ kind: "confirm_close", tab: 1, lines: ['close 1 "docs"? (y/n)'] });
    fireEvent.keyDown(root, { key: "Shift" });
    fireEvent.keyDown(root, { key: "y" });
    expect(lastOfType("close_tab")).toMatchObject({ tab: 1 });
  });

  /** `<leader>bo` (Owner answers Q2): the cross-task gap the fix-round-1 review found -- Rust's
   *  `confirm_close_others` had no handler at all, and no `close_others` message was ever posted
   *  back. The prompt is the band's own `prompt` fact now (panel round 2 plan, Task 10; `.band-prompt`,
   *  taking the whole band right of the mode section, spec §5.3.4), but `y` must post `close_others`,
   *  never `close_tab` -- there is no single tab to name. */
  it("<leader>bo prompts, and y sends close_others rather than close_tab", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")!;

    dispatch({ kind: "confirm_close_others", tabs: [2, 3], lines: ["close 2 other tabs? 1 running (y/n)"] });
    expect(container.querySelector(".band-prompt")!.textContent).toBe("close 2 other tabs? 1 running (y/n)");
    fireEvent.keyDown(root, { key: "n" });
    expect(lastOfType("close_others")).toBeUndefined();
    expect(container.querySelector(".band-prompt")).toBeNull();

    dispatch({ kind: "confirm_close_others", tabs: [2, 3], lines: ["close 2 other tabs? 1 running (y/n)"] });
    fireEvent.keyDown(root, { key: "y" });
    expect(lastOfType("close_others")).toBeDefined();
    expect(lastOfType("close_tab")).toBeUndefined();
    expect(container.querySelector(".band-prompt")).toBeNull();
    vi.unstubAllGlobals();
  });

  /** A rename field opened on one tab stays open across a switch (ruling: n/p/digits/l move without
   *  moving focus, D3 B) -- nothing here enforces mutual exclusion between renaming and switching.
   *  Committing must still name the tab the field is open on, never whichever tab is active by then. */
  it("commits a rename to the tab it was opened on, even after the active tab has changed", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "begin_rename", tab: 1, current: null });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    // The active tab changes while the rename field on tab 1 is still mounted.
    dispatch({ kind: "tabs", active: 2, tabs: two });
    fireEvent.change(input, { target: { value: "notes" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(lastOfType("rename_tab")).toMatchObject({ tab: 1, name: "notes" });
  });
});

describe("the session chooser", () => {
  const ENV = {
    kind: "chooser",
    open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }],
    records: [{ providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false }],
  };

  it("opens over the empty tab; Enter on a record resumes it in the active tab", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch(ENV);
    const chooser = container.querySelector<HTMLElement>(".chooser")!;
    fireEvent.keyDown(chooser, { key: "j" });
    fireEvent.keyDown(chooser, { key: "Enter" });
    expect(lastOfType("resume")).toMatchObject({ tab: 1, provider_session_id: "free-0000" });
    expect(container.querySelector(".chooser")).toBeNull();
  });

  /** GUI pass, 2026-09-25: `prefix w` over an empty tab, then `Esc`, left the keys on
   *  `.agent-ui-root` -- the empty layout's anchor, which handles no key. Typing went nowhere and
   *  not even `i` recovered it; only a click did. Wave 4 R2: D10's launch chooser is gone, so `Esc`
   *  here always returns the keys this way, entirely locally -- there is no round trip to Rust for
   *  it any more. **Correction (R16, spec §4.1):** the empty tab's live control used to be its
   *  composer, landing INPUT; over an empty tab `Esc`/`q` now returns to the dashboard menu in
   *  BROWSE instead (`Esc` never lands INPUT anywhere else in this panel either), reusing the same
   *  `arrive` pair panel round 2's decision 4 already sends for every other keyboard arrival. */
  it("Esc from prefix w over an empty tab returns to the dashboard menu, in BROWSE", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch(ENV);
    const postsBefore = posted.length;
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "Escape" });
    // V1 C1 (spec §3.5): the chooser's own close is still local -- no round trip to Rust for IT --
    // and the mirror posts once, BROWSE: this landing keeps BROWSE throughout (unlike the old
    // INPUT-landing behaviour this test used to pin), so there is no second, mode-changing post.
    expect(posted.slice(postsBefore)).toEqual([{ type: "panel_keys", request_id: expect.any(String), mode: "browse" }]);
    expect(container.querySelector(".chooser")).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
  });
});

describe("phase 3 lines, and the band's message/prompt (panel round 2 plan, Task 10)", () => {
  /** v1 (D6): a live tab always cycles now (see "Shift+Tab anywhere in the chat" below) -- the one
   *  case that still flashes rather than posting is a tab that has already ENDED and is not already
   *  in bypass, which can never be asked to ENTER bypass, only leave it. */
  it("flashes that the mode is fixed once a session has ended, and never offers a cycle into bypass (D6)", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "ended" }] });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
      act(() => widen(container));
      expect(container.querySelector(".status-band .mode-pill")!.textContent).toBe("⏵⏵ auto");
      const root = container.querySelector(".agent-ui-conversation")!;
      fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
      expect(container.querySelector(".band-message")!.textContent).toBe("the session has ended — r to start again");
      act(() => vi.advanceTimersByTime(2000));
      expect(container.querySelector(".band-message")).toBeNull();
      expect(lastOfType("cycle_mode")).toBeUndefined();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** Defect 2 (phase 1's sandbox pass): after `prefix r` the footer dropped the mode. A fresh
   *  document given the `ready` batch shows the tab's own mode. (Deviation from the task brief,
   *  recorded in the task report: this test already passed before Task 9's own App.tsx changes --
   *  phase 2's tab-held mode had already fixed it. It is kept here as the regression test ruling 2
   *  and this describe block ask for.) */
  it("shows the tab's mode after a reload's ready batch", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "history", entries: [] });
    dispatch({ kind: "editor_context", file: null, lines: null });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, mode: "bypass" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "queue", tab: 1, items: [], error: null });
    dispatch({ kind: "draft", tab: 1, text: "" });
    expect(container.querySelector(".status-band .mode-pill")!.textContent).toBe("⏵⏵ bypass permissions on");
  });

  /** The INPUT hints (F4) are gone (ruling R4) -- nothing replaces `.footer-hint`; the queue and the
   *  editor context both still show, the second now in the band's own `context` fact rather than a
   *  separate `ContextLine` between the queue and the composer. */
  it("draws the queue around the composer, and the editor context in the band", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 1);
    dispatch({ kind: "queue", tab: 1, items: [{ text: "and the tests", queuedAt: 1 }], error: null });
    dispatch({ kind: "editor_context", file: "src/a.rs", lines: [3, 9] });
    act(() => widen(container));
    enterInputMode(container);
    const order = Array.from(container.querySelector(".agent-ui-conversation")!.children).map((c) => c.className.split(" ")[0]);
    expect(order.indexOf("queue-lines")).toBeLessThan(order.indexOf("composer"));
    expect(container.querySelector(".context-line")).toBeNull();
    expect(container.querySelector(".footer-hint")).toBeNull();
    expect(container.querySelector(".band-context")!.textContent).toBe("⧉ src/a.rs:3-9");
    vi.unstubAllGlobals();
  });

  /** Defect 6 (phase 2's sandbox pass): the empty tab drew `prefix &`'s prompt as a bare span. */
  it("draws the empty tab's close prompt in its own band, like the conversation's", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    act(() => widen(container));
    dispatch({ kind: "confirm_close", tab: 1, lines: ['close 1 "new"? (y/n)'] });
    expect(container.querySelector(".status-band .band-prompt")!.textContent).toBe('close 1 "new"? (y/n)');
    vi.unstubAllGlobals();
  });
});

describe("P1: the keys land on a card that waits", () => {
  function oneCard() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "looking" },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: null, tool_name: "Bash", input: { command: "npm ci" } },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    return rendered;
  }

  /** Panel round 2 (spec §8, decision 4): `Ctrl+l` into the chat now sends `arrive`, not
   *  `enter_input` -- P1's own landing on a card waiting is unchanged either way. */
  it("Ctrl+l (arrive) with a card waiting lands in BROWSE on it, not in the composer", () => {
    const { container } = oneCard();
    dispatch({ kind: "arrive" });
    expect(container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode).toBe("browse");
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });

  /* v1 S4 (spec 2026-09-27 §2.2) reverses P1's "a answers the only card from any row" (ruling 26):
     `a` acts on the cursor's card only. The flash it shows instead is `v1: typing never answers a
     card`'s. The wait is long enough that S1's guard is not what refuses it. */
  it("a from another row does not answer the only card (v1 S4)", () => {
    vi.useFakeTimers();
    try {
      const { container } = oneCard();
      const root = container.querySelector(".agent-ui-conversation")!;
      fireEvent.keyDown(root, { key: "g" });
      fireEvent.keyDown(root, { key: "g" });
      expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(false);
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "a" });
      act(() => vi.advanceTimersByTime(1000));
      expect(lastOfType("permission_response")).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("a card arriving while typing leaves the mode alone and says how to answer it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1" }), 1);
    enterInputMode(container);
    dispatch({
      kind: "events", tab: 1, fromRevision: 1, throughRevision: 2,
      events: [{ type: "permission_requested", permission_id: "p", tool_use_id: null, tool_name: "Bash", input: { command: "rm x" } }],
    });
    expect(container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode).toBe("input");
    expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — Esc, j to it, a / d alone");
  });

  it("a switch to a tab holding a card lands on it while the panel has the keys", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({
      kind: "snapshot", tab: 2, throughRevision: 3,
      state: snapshotState({
        activeTurnId: "t",
        transcript: [{ seq: 1, text: "hello" }],
        pendingPermissions: [{ seq: 2, permissionId: "p2", toolUseId: null, toolName: "Write", input: { file_path: "a" } }],
      }),
    });
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });
});

describe("the first message from the empty tab (the phase-3 GUI pass, 2026-09-25)", () => {
  it("keeps the keys in INPUT once the session it started is live", () => {
    // Seen in the sandbox on both backends: the conversation mounted in BROWSE, so the next message
    // typed straight away ran as BROWSE keys -- `y` copied a row and `i` ate the rest of the word.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "pane_focus", focused: true });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "count to 40" } });
    fireEvent.keyDown(box, { key: "Enter" });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1", userPrompts: [{ seq: 1, text: "count to 40" }] }), 1);
    expect(container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode).toBe("input");
    expect(document.activeElement?.tagName).toBe("TEXTAREA");
  });
});

describe("the empty tab after a click on its background (the phase-3 GUI pass, 2026-09-25)", () => {
  it("a press on the root's own background does not take focus away from the composer", () => {
    // Seen in the sandbox beside a Lua panel: `.empty-tab` is centred (`margin: auto`), so a click
    // above or below it lands on `.agent-ui-root` (tabIndex -1), which took focus and handles no key:
    // Esc, i and typing all went nowhere until the pane was left and re-entered.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    const root = container.querySelector(".agent-ui-root") as HTMLElement;
    expect(fireEvent.mouseDown(root)).toBe(false);
    // Only its own background: a press on something inside it is left to the browser.
    expect(fireEvent.mouseDown(container.querySelector("textarea")!)).toBe(true);
  });
});

describe("P1 over a restored view (the phase-3 GUI pass, 2026-09-25)", () => {
  it("a switch back to a tab whose card waits below its saved view shows the card, not the saved scroll", () => {
    // Seen in the sandbox: `gg` in a tab with a card, `prefix n`, `prefix p` -- the cursor came back
    // on the card (25/25) but the view was restored to the top, so the card was off screen.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    const withCard = snapshotState({
      activeTurnId: "t",
      transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }],
      pendingPermissions: [{ seq: 3, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 4, state: withCard });
    dispatch({ kind: "pane_focus", focused: true });
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });

    const log: string[] = [];
    const list = container.querySelector(".message-list") as HTMLElement;
    let top = 0;
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => top,
      set: (v: number) => {
        top = v;
        log.push(`scrollTop=${v}`);
      },
    });
    const spy = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    spy.mockImplementation(function (this: Element) {
      log.push(`reveal ${this.classList.contains("row-permission") ? "card" : "other"}`);
    });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 4, state: withCard });
    spy.mockReset();
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    // Whatever else happens, the last thing to move the view must be the card's reveal.
    expect(log[log.length - 1]).toBe("reveal card");
  });
});

describe("a switch back to a tab whose reply streams (the small-defects GUI pass, 2026-09-25)", () => {
  const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
  const rowsUpTo = (n: number) =>
    snapshotState({ activeTurnId: "t", transcript: Array.from({ length: n }, (_, i) => ({ seq: i + 1, text: `row ${i + 1}` })) });
  const current = (container: HTMLElement) => container.querySelector(".row-current")?.textContent ?? "";

  it("a tab left while following comes back on the last row, even with rows that arrived meanwhile", () => {
    // Seen in the sandbox: `prefix c`, then `prefix p` while STREAM200 ran -- the cursor came back on
    // the row that was last when the tab was left, 27 rows above the end.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: rowsUpTo(3) });
    expect(current(container)).toContain("row 3");
    dispatch({ kind: "tabs", active: 2, tabs: two });
    const spy = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    spy.mockClear();
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 9, state: rowsUpTo(9) });
    expect(current(container)).toContain("row 9");
    // (The tab bar reveals its active tab; only a reveal inside the conversation moves the list.)
    const rowReveals = spy.mock.contexts.filter((el) => (el as Element).closest(".message-list") !== null);
    expect(rowReveals, "nothing reveals an older row, which would scroll the view up").toEqual([]);
    dispatch({ kind: "events", tab: 1, fromRevision: 9, throughRevision: 11, events: [
      { type: "assistant_message_boundary" },
      { type: "content_delta", turn_id: "t", kind: "text", text: "row 10" },
    ] });
    expect(current(container), "and rides the new last row").toContain("row 10");
  });

  it("a tab left while following comes back following", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: rowsUpTo(3) });
    dispatch({ kind: "tabs", active: 2, tabs: two });
    const seen: string[] = [];
    const onResume = () => seen.push("resume");
    const onUser = (e: Event) => seen.push(`user ${(e as CustomEvent).detail}`);
    document.addEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    document.addEventListener(USER_SCROLL_EVENT, onUser, true);
    try {
      dispatch({ kind: "tabs", active: 1, tabs: two });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 9, state: rowsUpTo(9) });
    } finally {
      document.removeEventListener(RESUME_FOLLOW_EVENT, onResume, true);
      document.removeEventListener(USER_SCROLL_EVENT, onUser, true);
    }
    // Following is restored outright, not left to a scroll event read inside a steering window --
    // where any later reveal above the end reads as the user scrolling up.
    expect(seen).toEqual(["resume"]);
  });

  it("a cursor move in the same render as a streamed delta is not undone", () => {
    // The mechanism behind the first test's parked view: R1's clamp moved the cursor while a delta
    // landed in the same render, the key reconciliation (for a run folding) read that as the row
    // having moved and put the cursor back, and its reveal scrolled the list up.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: rowsUpTo(3) });
    expect(current(container)).toContain("row 3");
    const root = container.querySelector(".agent-ui-conversation")!;
    act(() => {
      fireEvent.keyDown(root, { key: "k" });
      window.__neovibeDispatch!(
        JSON.stringify({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [
          { type: "content_delta", turn_id: "t", kind: "text", text: " more" },
        ] }),
      );
    });
    expect(current(container)).toContain("row 2");
  });
});

/** v1 audit P2-A4/R4: a digit prefix must not cost `O(count)` work, and must not simply keep
 *  growing forever either. Pure, DOM-free -- the accumulation function itself, not the keyboard
 *  wiring around it (that's the boundary-scan tests inside "BROWSE: counts…" below). */
describe("accumulateMotionCount (v1 audit R4)", () => {
  function typeCount(digits: string): number {
    let count: number | null = null;
    for (const ch of digits) count = accumulateMotionCount(count, Number(ch));
    return count!;
  }

  it("keeps an ordinary count exact", () => {
    expect(typeCount("3")).toBe(3);
    expect(typeCount("12")).toBe(12);
  });

  it("caps a count past MAX_MOTION_COUNT, far below vim's own silent overflow cap", () => {
    expect(typeCount("99999")).toBe(MAX_MOTION_COUNT);
    // Capped on every digit, not only once the whole string is read: a long run of the same digit
    // (someone holding a number key) never transiently builds a number past the cap either.
    expect(typeCount("9".repeat(50))).toBe(MAX_MOTION_COUNT);
  });
});

/** Task 3 (v1 picks, R2): where `gg`/`G` land, bare and counted. Pure -- the keyboard wiring around
 *  it is "counts: ..." in the scrolling describe. */
describe("jumpTarget (vim {N}G / {N}gg)", () => {
  it("is the first or the last row when bare", () => {
    expect(jumpTarget("first", null, 5)).toBe(0);
    expect(jumpTarget("last", null, 5)).toBe(4);
  });
  it("is row N, counted from 1, with a count -- gg and G alike", () => {
    expect(jumpTarget("last", 3, 5)).toBe(2);
    expect(jumpTarget("first", 3, 5)).toBe(2);
    expect(jumpTarget("last", 1, 5)).toBe(0);
    expect(jumpTarget("last", 5, 5)).toBe(4);
  });
  it("clamps a count past the last row onto it, as vim's {N}G does past the last line", () => {
    expect(jumpTarget("last", 6, 5)).toBe(4);
    expect(jumpTarget("first", 9999, 5)).toBe(4);
  });
  it("is row 0 when there are no rows, counted or not", () => {
    expect(jumpTarget("first", null, 0)).toBe(0);
    expect(jumpTarget("last", null, 0)).toBe(0);
    expect(jumpTarget("last", 3, 0)).toBe(0);
  });
});

describe("BROWSE: counts, prompt jumps and Ctrl+c", () => {
  it("3j moves three rows, and ]] / [[ go from prompt to prompt", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        userPrompts: [{ seq: 1, text: "first ask" }, { seq: 5, text: "second ask" }],
        transcript: [{ seq: 2, text: "alpha" }, { seq: 3, text: "bravo" }, { seq: 4, text: "charlie" }, { seq: 6, text: "delta" }],
      }),
      7,
    );
    const root = container.querySelector(".agent-ui-conversation")!;
    const current = () => container.querySelector(".row-current")!.textContent;
    // A reload's snapshot lands the cursor on the last row (the phase-3 GUI pass); start at the top.
    gg(container);
    fireEvent.keyDown(root, { key: "3" });
    fireEvent.keyDown(root, { key: "j" });
    expect(current()).toContain("charlie");
    fireEvent.keyDown(root, { key: "]" });
    fireEvent.keyDown(root, { key: "]" });
    expect(current()).toContain("second ask");
    fireEvent.keyDown(root, { key: "[" });
    fireEvent.keyDown(root, { key: "[" });
    expect(current()).toContain("first ask");
  });

  /** v1 audit P2-A4/R4: Codex's probe measured 3,004 DOM scans for `1000j` sitting at a one-row
   *  boundary (`nextStop`/`clampStep` clamp rather than returning `null` there, so the old loop's
   *  only early-exit condition never fired and it ran all `times` iterations). Fixed by "no
   *  progress = stop": once a repeat lands back on the stop the previous repeat already reached,
   *  the walk is over regardless of how many repeats are left. `querySelectorAll` is the DOM
   *  operation `stopsIn`/`conversationRows` (both walked by `nextStop`/`rowIndexOf`) actually run,
   *  same technique the saved audit probe used. */
  it("a large count at a one-row boundary does O(1) DOM scans, not O(count)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "the only row" }] }), 2);
    const root = container.querySelector(".agent-ui-conversation")!;
    gg(container);
    const scans = vi.spyOn(root, "querySelectorAll");
    for (const key of "1000") fireEvent.keyDown(root, { key });
    scans.mockClear();
    fireEvent.keyDown(root, { key: "j" });
    expect(scans.mock.calls.length).toBeLessThan(50);
    scans.mockRestore();
  });

  /** The same shape, past MAX_MOTION_COUNT -- proves the boundary early-exit alone already tames an
   *  even larger count (the cap above is the second, independent bound, tested in isolation). */
  it("an even larger count (past the cap) still does O(1) DOM scans", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "the only row" }] }), 2);
    const root = container.querySelector(".agent-ui-conversation")!;
    gg(container);
    const scans = vi.spyOn(root, "querySelectorAll");
    for (const key of "99999") fireEvent.keyDown(root, { key });
    scans.mockClear();
    fireEvent.keyDown(root, { key: "j" });
    expect(scans.mock.calls.length).toBeLessThan(50);
    scans.mockRestore();
  });

  /** v1 audit P2-A4/R4, round 2 (the Codex whole-branch review): the boundary tests above only
   *  prove a count that CANNOT move is cheap. A count that really walks was still `O(count)`: every
   *  step called `nextStop` -> `currentStop` -> `conversationRows` and then `rowIndexOf` ->
   *  `conversationRows`, so `1000j` from the first of 1,001 rows made 2,001 full-tree queries
   *  (about 2.3 s in jsdom). `countedStop` reads the tree once per motion. */
  it("1000j from the first of 1,001 rows reads the tree a bounded number of times and lands on the last", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const transcript = Array.from({ length: 1001 }, (_, i) => ({ seq: i + 1, text: `row ${i + 1}` }));
    dispatchLiveTab(snapshotState({ transcript }), 1002);
    const root = container.querySelector(".agent-ui-conversation")!;
    const current = () => container.querySelector(".row-current")!.textContent;
    gg(container);
    expect(current()).toContain("row 1");
    const scans = vi.spyOn(root, "querySelectorAll");
    for (const key of "1000") fireEvent.keyDown(root, { key });
    scans.mockClear();
    fireEvent.keyDown(root, { key: "j" });
    expect(scans.mock.calls.length).toBeLessThan(50);
    scans.mockRestore();
    expect(current()).toContain("row 1001");
  });

  /** Found writing `countedStop`: every step of the old walk re-read `document.activeElement`, so
   *  with the keys on a control inside a row (after `l` onto a card's Approve) each step started
   *  again from that row, and `3j` moved once. Vim's `3j` moves three. */
  it("3j from a card's Approve moves three stops, not one", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        transcript: [{ seq: 1, text: "alpha" }, { seq: 3, text: "bravo" }, { seq: 4, text: "charlie" }, { seq: 5, text: "delta" }],
        pendingPermissions: [{ seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
      }),
      6,
    );
    const root = container.querySelector(".agent-ui-conversation")!;
    const current = () => container.querySelector(".row-current")!;
    gg(container);
    fireEvent.keyDown(root, { key: "j" });
    expect(current().classList.contains("row-permission")).toBe(true);
    fireEvent.keyDown(root, { key: "l" });
    const approve = document.activeElement as HTMLElement;
    expect(approve.tagName).toBe("BUTTON");
    expect(current().contains(approve)).toBe(true);
    fireEvent.keyDown(approve, { key: "3" });
    fireEvent.keyDown(approve, { key: "j" });
    expect(current().textContent).toContain("delta");
  });

  it("Ctrl+c interrupts a running turn in BROWSE, even over a text selection", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1", transcript: [{ seq: 1, text: "select me" }] }), 2);
    const text = container.querySelector(".row-assistant")!;
    const range = document.createRange();
    range.selectNodeContents(text);
    window.getSelection()!.addRange(range);
    const root = container.querySelector(".agent-ui-conversation")!;
    expect(fireEvent.keyDown(root, { key: "c", ctrlKey: true })).toBe(false);
    expect(lastOfType("interrupt")).toMatchObject({ tab: 1 });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(posted.filter((m) => m.type === "interrupt")).toHaveLength(1);
  });
});

describe("R1: the cursor follows the view", () => {
  function rect(top: number, bottom: number) {
    return { top, bottom, left: 0, right: 100, width: 100, height: bottom - top, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
  }

  /** Review focus 5: the clamp moves the cursor, never the view. */
  it("the_clamp_never_writes_scrollTop and puts the cursor on the nearest visible row", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "zero" }, { seq: 2, text: "one" }, { seq: 3, text: "two" }] }), 4);
    // A reload's snapshot lands the cursor on the last row (the phase-3 GUI pass); start at the top.
    gg(container);
    const list = container.querySelector<HTMLElement>(".message-list")!;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    list.getBoundingClientRect = () => rect(0, 100);
    rows[0].getBoundingClientRect = () => rect(-300, -200);
    rows[1].getBoundingClientRect = () => rect(0, 50);
    rows[2].getBoundingClientRect = () => rect(50, 100);
    const writes = vi.fn();
    Object.defineProperty(list, "scrollTop", { configurable: true, get: () => 300, set: writes });
    fireEvent.scroll(list);
    expect(container.querySelector(".row-current")!.textContent).toContain("one");
    expect(writes).not.toHaveBeenCalled();
  });

  it("rides the last row while at the bottom, and lands on a prompt just sent", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "only" }] }), 2);
    const current = () => container.querySelector(".row-current")!.textContent;
    expect(current()).toContain("only");
    dispatch({ kind: "events", tab: 1, fromRevision: 2, throughRevision: 3, events: [{ type: "user_prompt_submitted", text: "next ask" }] });
    expect(current()).toContain("next ask");
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 5, events: [
      { type: "turn_started", turn_id: "t2" },
      { type: "content_delta", turn_id: "t2", kind: "text", text: "the reply" },
    ] });
    expect(current(), "at the bottom (jsdom reports 0 distance), the cursor rides the new last row").toContain("the reply");
  });
});

describe("R4: / and n / N", () => {
  function conversation() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({ transcript: ["alpha", "bravo", "charlie", "bravo again"].map((text, i) => ({ seq: i + 1, text })) }),
      5,
    );
    gg(rendered.container);
    return rendered;
  }
  const current = (c: HTMLElement) => c.querySelector(".row-current")!.textContent;

  it("moves as you type, keeps the match on Enter, and n / N step through the rest", () => {
    const { container } = conversation();
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "/" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    fireEvent.change(input, { target: { value: "bra" } });
    expect(current(container)).toContain("bravo");
    fireEvent.keyDown(input, { key: "Enter" });
    expect(container.querySelector(".search-bar")).toBeNull();
    fireEvent.keyDown(root, { key: "n" });
    expect(current(container)).toContain("bravo again");
    fireEvent.keyDown(root, { key: "n" });
    expect(current(container), "wraps").toContain("bravo");
    fireEvent.keyDown(root, { key: "N", shiftKey: true });
    expect(current(container)).toContain("bravo again");
  });

  it("puts the cursor back on Esc, ignores the IME's Enter, and says when nothing matches", () => {
    const widen = stubBandWidth();
    const { container } = conversation();
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "/" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    fireEvent.change(input, { target: { value: "charlie" } });
    expect(current(container)).toContain("charlie");
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(container.querySelector(".search-bar")).not.toBeNull();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(current(container)).toContain("alpha");
    fireEvent.keyDown(root, { key: "/" });
    fireEvent.change(container.querySelector<HTMLInputElement>(".search-bar input")!, { target: { value: "zulu" } });
    fireEvent.keyDown(container.querySelector<HTMLInputElement>(".search-bar input")!, { key: "Enter" });
    expect(container.querySelector(".band-message")!.textContent).toBe("pattern not found: zulu");
    vi.unstubAllGlobals();
  });

  it("n takes the keys back from a control that was refocused after a search", () => {
    // Found in review: `search-next` moved the row cursor via `setCursor` without ever calling
    // `root.focus()`, unlike every other cursor-moving action in this switch (`move`, `jump`,
    // `prompt-jump`, the Ctrl+d/Ctrl+u re-home). A stale `document.activeElement` on a permission
    // card's Approve button would then still answer a later Enter/Space, even though the visible
    // cursor had moved to a different row.
    const { container } = conversation();
    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 0,
      throughRevision: 1,
      events: [{ type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} }],
    });
    // cursor onto the permission card (last row), focus Approve
    fireEvent.keyDown(root, { key: "G", shiftKey: true });
    fireEvent.keyDown(root, { key: "l" });
    expect((document.activeElement as HTMLElement).getAttribute("data-nav-action")).toBe("allow");
    // search for a match elsewhere; Enter keeps it (already refocuses root on its own)
    fireEvent.keyDown(root, { key: "/" });
    fireEvent.change(container.querySelector<HTMLInputElement>(".search-bar input")!, { target: { value: "bravo" } });
    fireEvent.keyDown(container.querySelector<HTMLInputElement>(".search-bar input")!, { key: "Enter" });
    // back to the card, refocus Approve a second time
    fireEvent.keyDown(root, { key: "G", shiftKey: true });
    fireEvent.keyDown(root, { key: "l" });
    const approveBefore = document.activeElement;
    expect((approveBefore as HTMLElement).getAttribute("data-nav-action")).toBe("allow");
    fireEvent.keyDown(root, { key: "n" }); // repeat the search -- moves the cursor elsewhere
    expect(document.activeElement).not.toBe(approveBefore);
    expect(document.activeElement).toBe(root);
  });
});

describe("P2 runs and R3 the detailed view", () => {
  it("Enter on a collapsed run expands it, and Ctrl+o shows every result and survives a switch", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    const done = { content: "ok", isError: false };
    const state = snapshotState({
      toolCalls: [
        { seq: 1, toolUseId: "a", name: "Read", input: { file_path: "a.rs" }, result: done },
        { seq: 2, toolUseId: "b", name: "Read", input: { file_path: "b.rs" }, result: done },
        { seq: 3, toolUseId: "c", name: "Bash", input: { command: "ls" }, result: done },
      ],
      transcript: [{ seq: 4, text: "done" }],
    });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state });
    const root = container.querySelector(".agent-ui-conversation")!;
    // A reload's snapshot lands the cursor on the last row (the phase-3 GUI pass); start at the top.
    gg(container);
    expect(container.querySelectorAll(".row-tool-run")).toHaveLength(1);
    expect(container.querySelector(".row-tool-run")!.textContent).toContain("Read ×2 · Bash ×1");
    fireEvent.keyDown(root, { key: "Enter" });
    expect(container.querySelectorAll(".row-tool")).toHaveLength(3);
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(3);
    fireEvent.keyDown(root, { key: "o", ctrlKey: true });
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(0);
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state });
    expect(container.querySelectorAll(".row-tool")).toHaveLength(3);
    expect(container.querySelectorAll('[data-folded="true"]'), "the detailed view is the tab's own").toHaveLength(0);
  });

  /* v1 picks, Task 4 (vim `zo`/`zc`): the fold Enter flips on a collapsed run, opened and closed by
     name. `zc` closes the INNERMOST open fold -- a call's own open result before the run it sits in --
     and from any of the run's calls, the cursor following the run back (`indexOfKey` finds a call at
     its run). Two finished `Read`s, then a message, exactly as the Enter test above. */
  function twoReadsThenAMessage() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const done = { content: "ok", isError: false };
    dispatchLiveTab(
      snapshotState({
        toolCalls: [
          { seq: 1, toolUseId: "a", name: "Read", input: { file_path: "a.rs" }, result: done },
          { seq: 2, toolUseId: "b", name: "Read", input: { file_path: "b.rs" }, result: done },
        ],
        transcript: [{ seq: 3, text: "done" }],
      }),
      4,
    );
    gg(rendered.container);
    const root = rendered.container.querySelector(".agent-ui-conversation")!;
    const z = (second: string) => {
      fireEvent.keyDown(root, { key: "z" });
      fireEvent.keyDown(root, { key: second });
    };
    const summary = () => rendered.container.querySelector(".row-tool-run")?.textContent ?? null;
    const calls = () => rendered.container.querySelectorAll(".row-tool").length;
    return { ...rendered, root, z, summary, calls };
  }

  it("zo on a collapsed run expands it, and zc on either of its calls collapses it again", () => {
    const { container, root, z, summary, calls } = twoReadsThenAMessage();
    expect(summary()).toContain("Read ×2");

    z("o");
    expect(summary(), "zo opened the run").toBeNull();
    expect(calls()).toBe(2);

    // The cursor is on the run's first call, whose own result is closed: zc there closes the run again,
    // the cursor on it.
    expect(container.querySelector(".row-current")!.textContent).toContain("a.rs");
    z("c");
    expect(summary()).toContain("Read ×2");
    expect(calls()).toBe(0);
    expect(container.querySelector(".row-current")!.classList.contains("row-tool-run")).toBe(true);
    z("c");
    expect(summary(), "zc on a run that is closed leaves it closed").toContain("Read ×2");

    // ...and from its SECOND call the cursor follows the run back to where it folded.
    z("o");
    fireEvent.keyDown(root, { key: "j" });
    expect(container.querySelector(".row-current")!.textContent).toContain("b.rs");
    z("c");
    expect(summary()).toContain("Read ×2");
    expect(container.querySelector(".row-current")!.classList.contains("row-tool-run")).toBe(true);
  });

  it("zo on a call of an open run opens that call's own result, and the run stays open", () => {
    const { container, z, summary, calls } = twoReadsThenAMessage();
    z("o");
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(2);
    z("o"); // the cursor is on the first call now, not the run: the innermost closed fold is its result
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(1);
    expect(calls()).toBe(2);
    expect(summary()).toBeNull();
    z("o");
    expect(container.querySelectorAll('[data-folded="true"]'), "zo on an open result leaves it open").toHaveLength(1);
  });

  it("zc closes a call's own open result before the run it sits in", () => {
    const { container, root, z, summary, calls } = twoReadsThenAMessage();
    z("o");
    fireEvent.keyDown(root, { key: "Enter" }); // the first call's own result
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(1);

    z("c");
    expect(calls(), "the run is still open").toBe(2);
    expect(container.querySelectorAll('[data-folded="true"]'), "its result closed first").toHaveLength(2);
    z("c");
    expect(summary(), "the second zc closes the run").toContain("Read ×2");
  });

  it("Ctrl+o shows every call and zc folds nothing there -- not even a run opened before it", () => {
    const { container, root, z, summary, calls } = twoReadsThenAMessage();
    fireEvent.keyDown(root, { key: "o", ctrlKey: true });
    expect(calls()).toBe(2);
    expect(summary()).toBeNull();
    z("c");
    expect(calls(), "the detailed view has no run to fold").toBe(2);
    expect(summary()).toBeNull();
    fireEvent.keyDown(root, { key: "o", ctrlKey: true }); // back to the folded view: the run again
    expect(summary()).toContain("Read ×2");
    expect(container.querySelectorAll('[data-folded="true"]')).toHaveLength(0);

    // A run opened first, then the detailed view: zc there must leave it open for when Ctrl+o ends.
    z("o");
    expect(summary()).toBeNull();
    fireEvent.keyDown(root, { key: "o", ctrlKey: true });
    z("c");
    fireEvent.keyDown(root, { key: "o", ctrlKey: true });
    expect(summary(), "zc in the detailed view closed nothing").toBeNull();
    expect(calls()).toBe(2);
  });
});

describe("N3 and P5 in the conversation", () => {
  function withBash() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        activeTurnId: "t",
        toolCalls: [{ seq: 1, toolUseId: "tb", name: "Bash", input: { command: "cargo test" }, result: { content: "test result: ok", isError: false } }],
        pendingPermissions: [{ seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
      }),
      3,
    );
    gg(rendered.container);
    return rendered;
  }

  it("y copies the command and flashes the row; Y copies the output", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const clipboard = stubClipboard();
      const { container } = withBash();
      act(() => widen(container));
      const root = container.querySelector(".agent-ui-conversation")!;
      fireEvent.keyDown(root, { key: "y" });
      expect(clipboard.writeText).toHaveBeenLastCalledWith("cargo test");
      expect(container.querySelector(".row-current")!.classList.contains("row-yanked")).toBe(true);
      expect(container.querySelector(".band-message")!.textContent).toBe("copied 10 chars");
      act(() => vi.advanceTimersByTime(400));
      expect(container.querySelector(".row-yanked")).toBeNull();
      fireEvent.keyDown(root, { key: "Y", shiftKey: true });
      expect(clipboard.writeText).toHaveBeenLastCalledWith("test result: ok");
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("Y on a row with no output copies nothing and says so", () => {
    const widen = stubBandWidth();
    const clipboard = stubClipboard();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "hello" }] }), 2);
    act(() => widen(container));
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "Y", shiftKey: true });
    expect(clipboard.writeText).not.toHaveBeenCalled();
    expect(container.querySelector(".band-message")!.textContent).toBe("this row has no output");
    vi.unstubAllGlobals();
  });

  it("D puts the keys in the card's reason box; Enter there denies with the reason", () => {
    // v1 S4/S1 (spec 2026-09-27 §2.1-§2.2): `D` acts on the cursor's card only, after the wait.
    vi.useFakeTimers();
    try {
      const { container } = withBash();
      const root = container.querySelector(".agent-ui-conversation")!;
      fireEvent.keyDown(root, { key: "j" });
      expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "D", shiftKey: true });
      act(() => vi.advanceTimersByTime(250));
      const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
      expect(document.activeElement).toBe(reason);
      fireEvent.change(reason, { target: { value: "keep the build" } });
      fireEvent.keyDown(reason, { key: "Enter" });
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "deny", reason: "keep the build" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("the third button answers with remember, only when Rust offered a rule", () => {
    const { container } = withBash();
    expect(buttonLabelled(container, "Always allow")).toBeUndefined();
    dispatch({ kind: "rule_offers", tab: 1, offers: { p1: "rm *" } });
    fireEvent.click(buttonLabelled(container, "Always allow rm * in this project")!);
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "allow", remember: true });
  });
});

describe("N2 and R3: the editor round trips", () => {
  function withPaths(text: string) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text }] }), 2);
    return rendered;
  }

  it("gf with one path opens it at its line", () => {
    const { container } = withPaths("the bug is in `src/parser.rs:42`");
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "f" });
    expect(lastOfType("open_path")).toMatchObject({ path: "src/parser.rs", line: 42 });
  });

  it("gf with several paths lists them with letters, and the letter picks one", () => {
    const { container } = withPaths("compare a.rs and b.rs");
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "f" });
    expect(container.querySelector(".path-pick")!.textContent).toBe("a a.rs · s b.rs");
    fireEvent.keyDown(root, { key: "s" });
    expect(lastOfType("open_path")).toMatchObject({ path: "b.rs" });
    expect(container.querySelector(".path-pick")).toBeNull();
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "f" });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(container.querySelector(".path-pick")).toBeNull();
    expect(posted.filter((m) => m.type === "open_path")).toHaveLength(1);
  });

  /** v1 audit P2-A5: a `gf` picker left open over tab 1's conversation named paths that meant
   *  nothing once the panel switched to tab 2 -- the very next letter typed there still opened one
   *  of the old tab's paths, because the tab-switch reset cleared a long, explicit list of
   *  per-conversation UI state and `pathPick` was not on it. Codex's saved probe reproduces this
   *  exactly (`/scratch/v1-audit-probes/neovibe-p2/P2.audit.test.tsx`, "P2 cancels the path
   *  picker on a tab switch"). */
  it("switching tabs while gf's picker is open drops it: no open_path on the new tab (P2-A5)", () => {
    const { container } = withPaths("compare a.rs and b.rs");
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "f" });
    expect(container.querySelector(".path-pick")).not.toBeNull();
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "second conversation" }] }) });
    expect(container.querySelector(".path-pick")).toBeNull();
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "a" });
    expect(posted.filter((m) => m.type === "open_path")).toEqual([]);
  });

  it("a click on an inline code path opens it", () => {
    const { container } = withPaths("see `core/src/scratch.rs`");
    fireEvent.click(container.querySelector(".row-assistant code")!);
    expect(lastOfType("open_path")).toMatchObject({ path: "core/src/scratch.rs" });
  });

  it("Ctrl+g sends the row's whole text, and a refusal flashes in the band", () => {
    const widen = stubBandWidth();
    const { container } = withPaths("a long answer");
    act(() => widen(container));
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "g", ctrlKey: true });
    const sent = lastOfType("view_in_editor")!;
    expect(sent).toMatchObject({ text: "a long answer" });
    dispatch({ kind: "command_result", requestId: sent.request_id, ok: false, error: "the editor is not ready yet" });
    expect(container.querySelector(".band-message")!.textContent).toBe("the editor is not ready yet");
    expect(container.querySelector(".command-notice")).toBeNull();
    vi.unstubAllGlobals();
  });
});

/* v1 picks, Task 8 (ruling R6): `gx` opens the web link of the row under the cursor. One link whose
   visible text IS its address opens at once; several, or a titled one, or one nobody can see, wait for a
   letter with each full address shown (Review Focus 3: a link in a model's reply is never opened without
   the reader having seen where it goes). The page posts `open_url` with the WHATWG-normalized `href`;
   Rust re-checks it (`agent_panel.rs#web_url`). jsdom lays nothing out, so `boxLinks` gives the
   conversation, the list and each visible anchor a box -- what `hintVisible` reads as "on screen". */
describe("gx: web links on a row (v1 picks, Task 8, R6)", () => {
  function withReply(text: string) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text }] }), 2);
    return rendered;
  }
  const boxOf = (el: Element, top: number, height: number) => {
    (el as HTMLElement).getBoundingClientRect = () =>
      ({ top, bottom: top + height, left: 0, right: 100, width: 100, height, x: 0, y: top }) as DOMRect;
  };
  /** Every anchor that is not `hidden` is on screen, one below the other. */
  function boxLinks(container: HTMLElement) {
    boxOf(container.querySelector(".agent-ui-conversation")!, 0, 1000);
    boxOf(container.querySelector(".message-list")!, 0, 1000);
    let y = 0;
    for (const a of container.querySelectorAll(".row a[href]:not([hidden])")) {
      boxOf(a, y, 10);
      y += 10;
    }
  }
  const rootOf = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const gx = (container: HTMLElement) => {
    fireEvent.keyDown(rootOf(container), { key: "g" });
    fireEvent.keyDown(rootOf(container), { key: "x" });
  };
  const opened = () => posted.filter((m) => m.type === "open_url");
  const pick = (c: HTMLElement) => c.querySelector(".link-pick");

  it("opens at once a link whose text is its own address, with no pick", () => {
    const { container } = withReply("see https://example.com/a");
    boxLinks(container);
    gx(container);
    expect(lastOfType("open_url")).toMatchObject({ type: "open_url", url: "https://example.com/a" });
    expect(typeof lastOfType("open_url")!.request_id).toBe("string");
    expect(pick(container)).toBeNull();
    expect(opened()).toHaveLength(1);
  });

  it("opens a bare host at once, sending the address the browser will use (a trailing slash)", () => {
    const { container } = withReply("see https://example.com");
    boxLinks(container);
    gx(container);
    expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/" });
  });

  it("ignores a count, as gf does: 3gx is gx", () => {
    const { container } = withReply("see https://example.com/a");
    boxLinks(container);
    fireEvent.keyDown(rootOf(container), { key: "3" });
    gx(container);
    expect(opened()).toHaveLength(1);
  });

  it("lists several links with letters and their full normalized addresses, and the letter opens one", () => {
    const { container } = withReply("see https://example.com/a and [docs](https://EXAMPLE.com/b)");
    boxLinks(container);
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://example.com/a · s https://example.com/b");
    expect(opened()).toEqual([]);
    fireEvent.keyDown(rootOf(container), { key: "s" });
    expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/b" });
    expect(pick(container)).toBeNull();
    expect(opened()).toHaveLength(1);
  });

  it("lists at most as many links as there are letters", () => {
    const links = Array.from({ length: 16 }, (_, i) => `[l${i}](https://example.com/${i})`).join(" ");
    const { container } = withReply(links);
    boxLinks(container);
    gx(container);
    const shown = pick(container)!.textContent!.split(" · ");
    expect(shown).toHaveLength(14);
    expect(shown[0]).toBe("a https://example.com/0");
    expect(shown[13]).toBe("o https://example.com/13");
    // A letter past the list opens nothing and ends the pick.
    fireEvent.keyDown(rootOf(container), { key: "o" });
    expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/13" });
  });

  it("shows a titled link's address instead of opening it: text is not where it goes", () => {
    const { container } = withReply("read [docs](https://evil.example/x)");
    boxLinks(container);
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://evil.example/x");
    expect(opened()).toEqual([]);
    fireEvent.keyDown(rootOf(container), { key: "a" });
    expect(lastOfType("open_url")).toMatchObject({ url: "https://evil.example/x" });
  });

  it("shows the address a look-alike host really opens (punycode), never as typed", () => {
    // Raw HTML, so the `href` reaches the page exactly as the reply wrote it (a markdown link's is
    // percent-encoded by marked, which would already differ from its text): a Cyrillic "a" in apple.com.
    const { container } = withReply('<a href="https://\u0430pple.com/">https://\u0430pple.com/</a>');
    boxLinks(container);
    const anchor = container.querySelector(".row a[href]")!;
    expect(anchor.getAttribute("href")).toBe("https://\u0430pple.com/");
    expect(anchor.textContent).toBe(anchor.getAttribute("href"));
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://xn--pple-43d.com/");
    expect(opened()).toEqual([]);
    fireEvent.keyDown(rootOf(container), { key: "a" });
    expect(lastOfType("open_url")).toMatchObject({ url: "https://xn--pple-43d.com/" });
  });

  it("shows a link nobody can see, even when its text is its address (a reply can carry a hidden anchor)", () => {
    const { container } = withReply('<a hidden href="https://evil.example/">https://evil.example/</a>');
    boxLinks(container); // the hidden one is left unboxed, as a browser measures it
    expect(container.querySelector(".row a[hidden]")).not.toBeNull();
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://evil.example/");
    expect(opened()).toEqual([]);
  });

  it("shows a link whose visible text is only part of its address: hidden markup inside the text is not what the reader sees", () => {
    // Fix round 1 (review, and Codex): DOMPurify keeps `hidden` and inline HTML inside link text, and
    // `textContent` counts a hidden span -- so a reply could show `https://good.example/`, carry the real
    // address `https://good.example.evil.example/` in the text too, and pass a text-equals-href test.
    for (const reply of [
      '<a href="https://good.example.evil.example/">https://good.example<span hidden>.evil.example</span>/</a>',
      "[https://good.example<span hidden>.evil.example</span>/](https://good.example.evil.example/)",
    ]) {
      posted = [];
      const { container, unmount } = withReply(reply);
      boxLinks(container);
      const anchor = container.querySelector(".row a[href]")!;
      // The setup: the hidden span survived sanitizing, and the text reads as the real address to a test
      // that only looks at `textContent`.
      expect(anchor.querySelector("span[hidden]"), reply).not.toBeNull();
      expect(anchor.textContent, reply).toBe(anchor.getAttribute("href"));
      gx(container);
      expect(opened(), reply).toEqual([]);
      expect(pick(container)!.textContent, reply).toBe("a https://good.example.evil.example/");
      fireEvent.keyDown(rootOf(container), { key: "a" });
      expect(lastOfType("open_url"), reply).toMatchObject({ url: "https://good.example.evil.example/" });
      unmount();
    }
  });

  it("shows a link whose text sits inside another element, even when it reads as its address", () => {
    const { container } = withReply('<a href="https://example.com/a"><b>https://example.com/a</b></a>');
    boxLinks(container);
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://example.com/a");
    expect(opened()).toEqual([]);
  });

  it("never lets a link out of view open on its own: scrolled off the list, it waits for a letter", () => {
    const { container } = withReply("see https://example.com/a");
    boxLinks(container);
    boxOf(container.querySelector(".row a[href]")!, 5000, 10); // far below the list's box
    gx(container);
    expect(pick(container)!.textContent).toBe("a https://example.com/a");
    expect(opened()).toEqual([]);
  });

  it("says there is no web link on a row with only relative or non-web ones, and posts nothing", () => {
    const widen = stubBandWidth();
    try {
      const { container } = withReply("see [docs](docs/a.md), [own](https://neovibe.invalid/x) and [mail](mailto:a@b)");
      act(() => widen(container));
      boxLinks(container);
      gx(container);
      expect(container.querySelector(".band-message")!.textContent).toBe("no web link on this row");
      expect(pick(container)).toBeNull();
      expect(opened()).toEqual([]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("says so on a row that has no link at all", () => {
    const widen = stubBandWidth();
    try {
      const { container } = withReply("nothing to open here");
      act(() => widen(container));
      gx(container);
      expect(container.querySelector(".band-message")!.textContent).toBe("no web link on this row");
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("finds no link in your own prompt or a tool's output, which are plain text", () => {
    const widen = stubBandWidth();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState(), 0);
      dispatch({
        kind: "events",
        tab: 1,
        fromRevision: 0,
        throughRevision: 3,
        events: [
          { type: "user_prompt_submitted", text: "open https://example.com/a for me" },
          { type: "turn_started", turn_id: "t1" },
          { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "echo https://example.com/b" } },
        ],
      });
      act(() => widen(container));
      boxLinks(container);
      expect(container.querySelector(".row a")).toBeNull();
      gg(container); // the prompt
      gx(container);
      expect(container.querySelector(".band-message")!.textContent).toBe("no web link on this row");
      fireEvent.keyDown(rootOf(container), { key: "G", shiftKey: true }); // the tool call
      gx(container);
      expect(container.querySelector(".band-message")!.textContent).toBe("no web link on this row");
      expect(opened()).toEqual([]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("reads only the row under the cursor, not the other rows' links", () => {
    const widen = stubBandWidth(); // before the render that mounts the band
    try {
      const rendered = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first https://example.com/first" }, { seq: 2, text: "second: no links" }] }), 2);
      act(() => widen(rendered.container));
      boxLinks(rendered.container);
      gx(rendered.container); // the cursor is on the last row, which has none
      expect(opened()).toEqual([]);
      expect(rendered.container.querySelector(".band-message")!.textContent).toBe("no web link on this row");
    } finally {
      vi.unstubAllGlobals();
    }
  });

  /* Review Focus 1, and the pick's own keys: only a plain letter chooses. A chord, an input method's key, or
     a held `x`'s auto-repeat is not a choice -- the first two end the pick and open nothing, the repeat
     leaves it waiting (it is the `x` of `gx` still held, not a second key). */
  describe("the pick's keys", () => {
    function openTwo() {
      const rendered = withReply("see https://example.com/a and [docs](https://example.com/b)");
      boxLinks(rendered.container);
      gx(rendered.container);
      expect(pick(rendered.container)).not.toBeNull();
      return rendered;
    }

    it("closes on Ctrl+a, Alt+s, Meta+s and an input method's key, and opens nothing", () => {
      for (const ev of [
        { key: "a", ctrlKey: true },
        { key: "s", altKey: true },
        { key: "s", metaKey: true },
        { key: "a", isComposing: true },
        { key: "a", keyCode: 229 },
      ]) {
        posted = [];
        const { container, unmount } = openTwo();
        fireEvent.keyDown(rootOf(container), ev);
        expect(pick(container), JSON.stringify(ev)).toBeNull();
        expect(opened(), JSON.stringify(ev)).toEqual([]);
        unmount();
      }
    });

    it("does not treat an uppercase letter, a digit or Enter as a choice", () => {
      for (const ev of [{ key: "A", shiftKey: true }, { key: "1" }, { key: "Enter" }, { key: "Escape" }, { key: "j" }]) {
        posted = [];
        const { container, unmount } = openTwo();
        fireEvent.keyDown(rootOf(container), ev);
        expect(pick(container), JSON.stringify(ev)).toBeNull();
        expect(opened(), JSON.stringify(ev)).toEqual([]);
        unmount();
      }
    });

    it("leaves the pick waiting through a held x's repeat and a bare modifier", () => {
      const { container } = openTwo();
      fireEvent.keyDown(rootOf(container), { key: "x", repeat: true });
      fireEvent.keyDown(rootOf(container), { key: "Shift", shiftKey: true });
      fireEvent.keyDown(rootOf(container), { key: "Control", ctrlKey: true });
      expect(pick(container)).not.toBeNull();
      fireEvent.keyDown(rootOf(container), { key: "a" });
      expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/a" });
      expect(pick(container)).toBeNull();
    });

    it("swallows the key that ends it: a plain letter never also runs as a BROWSE key", () => {
      const { container } = openTwo();
      const notPrevented = fireEvent.keyDown(rootOf(container), { key: "j" });
      expect(notPrevented).toBe(false);
    });
  });

  /* Every route away closes it (`dropPendingKeys`, and the tab switch beside `setPathPick(null)`): a pick left
     up would open a link on a later letter, over a conversation that no longer holds it. */
  describe("every route away closes the pick", () => {
    function openTwo() {
      const rendered = withReply("see https://example.com/a and [docs](https://example.com/b)");
      boxLinks(rendered.container);
      gx(rendered.container);
      expect(pick(rendered.container)).not.toBeNull();
      return rendered;
    }
    const later = (container: HTMLElement) => {
      expect(pick(container)).toBeNull();
      fireEvent.keyDown(rootOf(container), { key: "a" });
      expect(opened()).toEqual([]);
    };

    it("pane_focus false (the keys left the panel)", () => {
      const { container } = openTwo();
      dispatch({ kind: "pane_focus", focused: false });
      later(container);
    });

    it("a global HINT starting", () => {
      const { container } = openTwo();
      dispatch({ kind: "hint_collect", sessionId: 1 });
      later(container);
    });

    it("an arrival by keyboard", () => {
      const { container } = openTwo();
      dispatch({ kind: "arrive" });
      later(container);
    });

    it("Ctrl+j/Ctrl+k, which GTK takes before the page", () => {
      const { container } = openTwo();
      dispatch({ kind: "nav_key", direction: "down" });
      later(container);
    });

    /* Fix round 1 (Codex): the pick owns every key ahead of the key table, so anything else that takes the
       keys must end it -- or the next letter typed there would open a link instead of being typed. */
    it("the composer taking the keys (a click into it): its letters are typed, and open nothing", () => {
      const { container } = openTwo();
      act(() => container.querySelector<HTMLElement>(".composer-browse-hint")!.focus());
      const textarea = container.querySelector("textarea")!;
      expect(pick(container)).toBeNull();
      expect(fireEvent.keyDown(textarea, { key: "a" })).toBe(true); // not prevented: the letter is typed
      expect(opened()).toEqual([]);
    });

    it("a text field that has the keys with the composer still in BROWSE (a card's reason box)", () => {
      const rendered = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(
        snapshotState({
          transcript: [{ seq: 1, text: "see https://example.com/a and [docs](https://example.com/b)" }],
          pendingPermissions: [{ seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
        }),
        2,
      );
      gg(rendered.container);
      boxLinks(rendered.container);
      gx(rendered.container);
      expect(pick(rendered.container)).not.toBeNull();
      const reason = rendered.container.querySelector<HTMLInputElement>(".row-permission input")!;
      act(() => reason.focus());
      // A letter typed there ends the pick and is the text field's own: not prevented, no link opened.
      expect(fireEvent.keyDown(reason, { key: "a" })).toBe(true);
      expect(pick(rendered.container)).toBeNull();
      expect(opened()).toEqual([]);
      expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
    });

    it("the ? overlay opening (prefix ?, which GTK takes)", () => {
      const { container } = openTwo();
      dispatch({ kind: "open_keymap" });
      expect(container.querySelector(".keymap-overlay")).not.toBeNull();
      later(container);
    });

    it("an overlay drawn over the conversation (the chooser)", () => {
      const { container } = openTwo();
      dispatch({
        kind: "chooser",
        open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: false }],
        records: [],
      });
      expect(container.querySelector(".chooser")).not.toBeNull();
      expect(pick(container)).toBeNull();
    });

    it("switching tabs: no open_url on the new tab", () => {
      const { container } = openTwo();
      const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
      dispatch({ kind: "tabs", active: 2, tabs: two });
      dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [{ seq: 1, text: "second conversation" }] }) });
      later(container);
    });
  });

  it("tells Rust the panel does not hold the keys for its own composer while the pick waits", () => {
    const { container } = withReply("see https://example.com/a and [docs](https://example.com/b)");
    boxLinks(container);
    dispatch({ kind: "pane_focus", focused: true });
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "browse" });
    gx(container);
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "other" });
    fireEvent.keyDown(rootOf(container), { key: "Escape" });
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "browse" });
  });

  it("flashes a refusal in the band, never in the banner that means the conversation is broken", () => {
    const widen = stubBandWidth();
    try {
      const { container } = withReply("see https://example.com/a");
      act(() => widen(container));
      boxLinks(container);
      gx(container);
      const sent = lastOfType("open_url")!;
      dispatch({ kind: "command_result", requestId: sent.request_id, ok: false, error: "not a web link" });
      expect(container.querySelector(".band-message")!.textContent).toBe("not a web link");
      expect(container.querySelector(".command-notice")).toBeNull();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("works on a session that has ended, as gf does", () => {
    const { container } = withReply("see https://example.com/a");
    boxLinks(container);
    dispatch({ kind: "events", tab: 1, fromRevision: 2, throughRevision: 3, events: [{ type: "session_closed", reason: "provider exited" }] });
    boxLinks(container);
    gx(container);
    expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/a" });
  });

  /* The pick's letters include `a` and `d`, which answer a card in BROWSE. The pick is modal -- its keys are
     read ahead of the key table -- so with a card waiting elsewhere in the conversation, `a` opens the first
     link and never allows the card, and `d` the second and never denies it. */
  it("is not a card answer: with a card waiting, the pick's a and d open the first and third link and answer nothing", () => {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        transcript: [{ seq: 1, text: "see https://example.com/a, [docs](https://example.com/b) and [more](https://example.com/c)" }],
        pendingPermissions: [{ seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
      }),
      2,
    );
    gg(rendered.container);
    boxLinks(rendered.container);
    expect(rendered.container.querySelector(".row-current")!.classList.contains("row-assistant")).toBe(true);
    gx(rendered.container);
    fireEvent.keyDown(rootOf(rendered.container), { key: "a" });
    expect(lastOfType("open_url")).toMatchObject({ url: "https://example.com/a" });
    gx(rendered.container);
    fireEvent.keyDown(rootOf(rendered.container), { key: "d" });
    expect(opened().map((m) => m.url)).toEqual(["https://example.com/a", "https://example.com/c"]);
    expect(posted.filter((m) => m.type === "permission_response")).toEqual([]);
    expect(rendered.container.querySelector(".row-permission")).not.toBeNull();
  });
});

/** Moved in from the deleted `statusRow.test.ts` (panel round 2 plan, Task 10): `statusWarning` and
 *  its `SKEW_PREFIX` constant moved into `App.tsx` itself, since the band's `warn` fact stays
 *  decoupled from `ProviderInfo` (`band.ts` takes only the already-computed string). */
describe("statusWarning (D12 A, ruling 9)", () => {
  const provider = (diagnostics: string[]): ProviderInfo => ({
    sidecarVersion: "0.9",
    claudeAgentSdkVersion: "0.2",
    claudeCodeVersion: "2.1.282",
    protocol: "3.4",
    buildDescription: "Verdandi checkout: /v @ 28a5e4c (via default path)",
    startupDiagnostics: diagnostics,
  });

  it("warns for Verdandi skew", () => {
    const drift = "Verdandi baseline drift: running 1234567, this client was verified against 28a5e4c.";
    expect(statusWarning(provider([drift]), null)).toBe(drift);
  });
  it("warns for a refusal, which is a tab that failed to start", () => {
    expect(statusWarning(null, "the CLI gate refused 2.1.999")).toBe("the CLI gate refused 2.1.999");
  });
  it("does not warn for the routine CLI version diagnostic, which goes to prefix i only", () => {
    expect(
      statusWarning(provider(["[claude-sidecar] CLI version diagnostic: 2.1.282 is in range but untested"]), null),
    ).toBeNull();
    expect(statusWarning(provider([]), null)).toBeNull();
    expect(statusWarning(null, null)).toBeNull();
  });
});

/** Wave 3 Task 1 (launch-chooser bug investigation, `~/.cache/launch-chooser-bug/`): the panel never
 *  strands the keys. An overlay -- the chooser, a tab rename -- keeps them (`pane_focus`/`arrive`
 *  re-focus it rather than falling through to the conversation or a composer under it), and a
 *  keyboard arrival on the empty tab always lands *somewhere* focusable (defect 4) even when Rust
 *  cannot hand the keys to the editor (defect 3). jsdom never drops DOM focus on its own, so a
 *  `pane_focus false` -> a real blur -> `pane_focus true` sequence simulates the WebView losing DOM
 *  focus across a GTK round trip -- without the blur these would pass today and prove nothing. */
describe("takes the keys", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }
  function loseThenRegainFocus() {
    dispatch({ kind: "pane_focus", focused: false });
    act(() => (document.activeElement as HTMLElement | null)?.blur());
    dispatch({ kind: "pane_focus", focused: true });
  }
  function openChooser() {
    dispatch({
      kind: "chooser",
      // Two rows besides "New session": tab 1 is the active one, so the chooser's own initial
      // cursor already sits there (`Chooser.tsx`'s "starts on the active tab's row") -- a second
      // row (a record) gives `j` somewhere to move to.
      open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: false }],
      records: [{ providerSessionId: "held-0000", name: null, title: "held one", createdAt: "1", updatedAt: "2", heldElsewhere: false }],
    });
  }

  it("a. chooser open: pane_focus losing and regaining focus re-focuses it, and j moves its cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    expect(container.querySelector(".chooser")).not.toBeNull();
    loseThenRegainFocus();
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
    const before = container.querySelector(".chooser-row.current")!.textContent;
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    expect(container.querySelector(".chooser-row.current")!.textContent).not.toBe(before);
  });

  /** rc.3 minors (K01): the chooser's own lone-`g` wait is a pending key too. A focus round trip is one of the
   *  window's cancel routes (`dropPendingKeys`), so the `j` after it moves the cursor instead of being
   *  swallowed as the cancel of a `g` typed before the panel lost the keys; Shift+Tab ends it the same way. */
  it("a2. chooser open: a waiting g does not outlive a focus round trip, and j moves the cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    fireEvent.keyDown(document.activeElement!, { key: "g" });
    loseThenRegainFocus();
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
    const before = container.querySelector(".chooser-row.current")!.textContent;
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    expect(container.querySelector(".chooser-row.current")!.textContent).not.toBe(before);
  });

  it("a3. chooser open: a waiting g does not outlive Shift+Tab, and j moves the cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    fireEvent.keyDown(document.activeElement!, { key: "g" });
    fireEvent.keyDown(document.activeElement!, { key: "Tab", shiftKey: true });
    const before = container.querySelector(".chooser-row.current")!.textContent;
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    expect(container.querySelector(".chooser-row.current")!.textContent).not.toBe(before);
  });

  /** rc.3 minors fix round: `App.tsx` renders the chooser at two sites -- over a live conversation (a2/a3
   *  above) and over an empty tab -- and only the first was covered, so the second one's `dropKeysRequest`
   *  could have been deleted with every test green. Same two routes, on the empty-tab layout. */
  it("a2e. chooser open over an empty tab: a waiting g does not outlive a focus round trip, and j moves the cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    fireEvent.keyDown(document.activeElement!, { key: "g" });
    loseThenRegainFocus();
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
    const before = container.querySelector(".chooser-row.current")!.textContent;
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    expect(container.querySelector(".chooser-row.current")!.textContent).not.toBe(before);
  });

  it("a3e. chooser open over an empty tab: a waiting g does not outlive Shift+Tab, and j moves the cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    fireEvent.keyDown(document.activeElement!, { key: "g" });
    fireEvent.keyDown(document.activeElement!, { key: "Tab", shiftKey: true });
    const before = container.querySelector(".chooser-row.current")!.textContent;
    fireEvent.keyDown(document.activeElement!, { key: "j" });
    expect(container.querySelector(".chooser-row.current")!.textContent).not.toBe(before);
  });

  it("b. chooser open with the filter open: pane_focus regaining focus re-focuses the filter input", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    fireEvent.keyDown(document.activeElement!, { key: "/" });
    const filterInput = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(document.activeElement).toBe(filterInput);
    loseThenRegainFocus();
    expect(document.activeElement).toBe(container.querySelector<HTMLInputElement>(".chooser-filter"));
  });

  it("c. chooser open over a live tab: enter_input never opens a textarea under it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    openChooser();
    dispatch({ kind: "enter_input" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".chooser")).not.toBeNull();
    expect(modeBlock(container).dataset.mode).toBe("browse");
  });

  it("c2. chooser open over an empty tab: enter_input never opens a textarea under it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    // Opening the chooser already moves the keys into it (its own mount focus), which blurs the
    // dashboard's textarea the same way any other focus steal does -- not the thing under test.
    openChooser();
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
    dispatch({ kind: "enter_input" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".chooser")).not.toBeNull();
  });

  it("d. chooser open: arrive keeps the focus in the chooser and does not move the conversation cursor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 2);
    dispatch({ kind: "pane_focus", focused: true });
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(container.querySelector(".row-current")!.textContent).toContain("first");
    openChooser();
    dispatch({ kind: "arrive" });
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
    expect(container.querySelector(".row-current")!.textContent).toContain("first");
  });

  /** Wave 4 R2: D10's launch chooser (and its flagged handoff to the editor on Esc) is gone, so the
   *  scenario "e" used to cover here -- Esc at launch, then Rust's `arrive` because the editor
   *  refused the keys -- can no longer happen: `Esc` always returns the keys locally now (see the
   *  "session chooser" describe block above), with no round trip to Rust at all. */
  it("f. empty tab, composer focused: arrive lands the keys on the empty tab root, not body", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    dispatch({ kind: "arrive" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".empty-tab")!.contains(document.activeElement)).toBe(true);
  });

  it("g. begin_rename open: pane_focus regaining focus re-focuses the tab bar's rename input", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "begin_rename", tab: 1, current: null });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    expect(document.activeElement).toBe(input);
    loseThenRegainFocus();
    expect(document.activeElement).toBe(container.querySelector<HTMLInputElement>(".tab-rename"));
  });

  /** Regression guard (passes today, must keep passing): a click landing focus on a control inside
   *  the panel -- Approve, here -- must not be yanked away by a `pane_focus true` that follows it. */
  it("h. pane_focus true does not steal focus from a control the user just clicked", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    const approve = container.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!;
    act(() => approve.focus());
    expect(document.activeElement).toBe(approve);
    dispatch({ kind: "pane_focus", focused: true });
    expect(document.activeElement).toBe(approve);
  });
});

/* Wave 3, Task 3. `MessageList`'s unread threshold used to be a timeline INDEX, latched at the
   moment following stopped and sliced off whatever the CURRENT timeline held. `MessageList` is not
   remounted across a tab switch (`App.tsx` reuses the one instance for every tab), so on a restore
   that index was recomputed against the just-restored tab's OWN (short) timeline -- every row that
   had actually arrived while the tab was away sat past that fresh cutoff and was silently never
   counted ("↓3" read "↓0", or showed nothing at all). The fix threads the threshold through as a
   `seq` instead: `App.tsx` saves it per tab (`TabViewState.unseenAfterSeq`) and hands it back to
   `MessageList` as `unseenSeed` on a restore, which seeds it in before the tab's real content -- and
   `MessageList`'s own `[state]` effect -- ever runs against it. */
describe("A parked tab's unread count includes what arrived while it was away (wave 3, Task 3)", () => {
  const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];

  /* jsdom implements no layout, so `.message-list`'s own scroll geometry is whatever these
     properties are told to be, the same fixture `MessageList.test.tsx`'s own pill tests use. */
  function setListScroll(list: HTMLElement, dims: { scrollHeight: number; clientHeight: number; scrollTop: number }) {
    Object.defineProperty(list, "scrollHeight", { value: dims.scrollHeight, configurable: true });
    Object.defineProperty(list, "clientHeight", { value: dims.clientHeight, configurable: true });
    Object.defineProperty(list, "scrollTop", { value: dims.scrollTop, configurable: true, writable: true });
  }

  it("counts the rows a parked tab gained while it was away", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({
      kind: "snapshot",
      tab: 1,
      throughRevision: 3,
      state: snapshotState({ transcript: [{ seq: 1, text: "a" }, { seq: 2, text: "b" }, { seq: 3, text: "c" }] }),
    });
    act(() => widen(container));
    const list = container.querySelector(".message-list") as HTMLElement;
    // Park it: scrolled up and away from the bottom, the max `seq` on screen (3) becomes the
    // threshold `MessageList` latches -- the same gesture the R2 pill tests in
    // `MessageList.test.tsx` use.
    setListScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    fireEvent.wheel(list, { deltaY: -100 });
    setListScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);

    // Switch away. `saveView` reads the DOM synchronously as PART of this dispatch (before dims are
    // touched again below), so it correctly captures the parked view above: `atBottom: false`,
    // `unseenAfterSeq: 3`.
    dispatch({ kind: "tabs", active: 2, tabs: two });
    // Tab 2's own (empty, one-line) conversation: a real browser lays it out much shorter than tab
    // 1 was -- distance collapses to (about) nothing, which is what actually re-arms `MessageList`'s
    // own following flag on a switch (`onScroll`'s `distance <= AT_BOTTOM_PX` branch). jsdom lays
    // nothing out on its own, so this is set by hand to match; without it, this test cannot tell the
    // fixed code from the broken code it replaces (checked below).
    setListScroll(list, { scrollHeight: 0, clientHeight: 400, scrollTop: 0 });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });

    // Tab 1's real content, laid out at its full (larger) size before the snapshot lands, matching
    // a real browser measuring new content as it mounts and BEFORE `App.tsx`'s own restore effect
    // gets a chance to move `scrollTop` back to the parked spot -- `MessageList`'s own `[state]`
    // effect, seeing `followingRef` still `true` from the tab-2 collapse above, snaps it to this
    // (wrong, unparked) bottom first.
    setListScroll(list, { scrollHeight: 5000, clientHeight: 400, scrollTop: 0 });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    // While away, tab 1's own conversation gained 3 more messages (seq 4-6), the same way the
    // "reply streams" tests above grow a tab's row count across a switch by handing a bigger
    // snapshot back on return.
    dispatch({
      kind: "snapshot",
      tab: 1,
      throughRevision: 6,
      state: snapshotState({
        transcript: [
          { seq: 1, text: "a" },
          { seq: 2, text: "b" },
          { seq: 3, text: "c" },
          { seq: 4, text: "d" },
          { seq: 5, text: "e" },
          { seq: 6, text: "f" },
        ],
      }),
    });
    // `App.tsx`'s own restore effect has, by now, really run (`list.scrollTop = view.scrollTop`,
    // 800 -- this is the product code, not a stub), correcting the snap above.
    expect(list.scrollTop).toBe(800);
    // But nothing has told `MessageList` about it yet: a plain `scrollTop` assignment fires no
    // synchronous `scroll` event, in a real browser or in jsdom. This `fireEvent.scroll` stands in
    // for the native one that eventually reaches `onScroll` once the browser catches up -- see
    // `MessageList`'s own seed-effect doc comment for why that ordering is exactly what the fix
    // relies on: without the seed effect, THIS is the event that would re-latch the threshold from
    // the just-restored (short) view instead of what was actually saved.
    act(() => widen(container));
    fireEvent.scroll(list);

    expect(container.querySelector(".band-unread")?.textContent).toBe("↓3");
    vi.unstubAllGlobals();
  });

  it("comes back following, with no unread label, when it was left at the bottom", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({
      kind: "snapshot",
      tab: 1,
      throughRevision: 3,
      state: snapshotState({ transcript: [{ seq: 1, text: "a" }, { seq: 2, text: "b" }, { seq: 3, text: "c" }] }),
    });
    act(() => widen(container));
    const list = container.querySelector(".message-list") as HTMLElement;
    // At the bottom: never scrolled up, so `saveView` records `atBottom: true` and no threshold.
    setListScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);

    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({
      kind: "snapshot",
      tab: 1,
      throughRevision: 6,
      state: snapshotState({
        transcript: [
          { seq: 1, text: "a" },
          { seq: 2, text: "b" },
          { seq: 3, text: "c" },
          { seq: 4, text: "d" },
          { seq: 5, text: "e" },
          { seq: 6, text: "f" },
        ],
      }),
    });
    act(() => widen(container));

    expect(container.querySelector(".band-unread")).toBeNull();
    vi.unstubAllGlobals();
  });
});

/** Wave 4 Task 1 (root cause and design: `.superpowers/sdd/2026-09-26-wave4/task-1-brief.md`). The
 *  owner: "shift tab还是不能切换，至少在browse区域是不可以，应该要做到在agent pane都要可以直接切换".
 *  A document-capture `keydown` listener (`App.tsx`'s `onModeKey`, `modeKey.ts`'s `modeKeyRoute`) now
 *  claims Shift+Tab ahead of every React handler and of WebKit's own default backward-focus
 *  navigation, wherever in the panel it lands -- these tests dispatch a real, bubbling `KeyboardEvent`
 *  the way a real keypress would arrive, not `fireEvent.keyDown` on a React element directly, so a
 *  gap in the router (rather than in a component's own handler) would actually show up here. */
describe("Shift+Tab anywhere in the chat (wave 4, Task 1)", () => {
  /** Dispatches a real, bubbling, cancelable Shift+Tab on `target` and returns the event so a test
   *  can read `defaultPrevented` off it -- `fireEvent`'s own return value is the inverse of that,
   *  which reads worse next to the other assertions each test makes on the same event. */
  function shiftTab(target: EventTarget) {
    const event = new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true });
    act(() => {
      target.dispatchEvent(event);
    });
    return event;
  }

  /** v1 (D6): the wave-5 `SetPermissionMode` capability this used to gate on is gone -- a `live`
   *  tab always cycles now, on any backend (a move INTO bypass gets its own `confirm_bypass` y/n
   *  from Rust instead, exercised in "v1 mode: entering bypass asks first" below). */
  it("a. live tab, BROWSE, focus on the conversation root: cycle_mode posted, no flash, focus unchanged", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")!;
    act(() => (root as HTMLElement).focus());
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(root);
    expect(container.querySelector(".band-message")).toBeNull();
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    vi.unstubAllGlobals();
  });

  it("b. live tab, INPUT, focus in the composer textarea: posted, text and focus kept", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    enterInputMode(container);
    const textarea = container.querySelector("textarea") as HTMLTextAreaElement;
    fireEvent.change(textarea, { target: { value: "keep me" } });
    const event = shiftTab(textarea);
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(textarea);
    expect(textarea.value).toBe("keep me");
    expect(container.querySelector(".band-message")).toBeNull();
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    vi.unstubAllGlobals();
  });

  it("c. empty tab, event target <body>: one cycle_mode posted with the active tab's id", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    const before = document.activeElement;
    const event = shiftTab(document.body);
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(before);
    const posts = posted.filter((m) => m.type === "cycle_mode");
    expect(posts).toHaveLength(1);
    expect(posts[0]).toMatchObject({ tab: 1 });
  });

  it("d. empty tab, focus on the start screen's .agent-ui-root: one cycle_mode", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    const root = container.querySelector(".agent-ui-root")!;
    act(() => (root as HTMLElement).focus());
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(root);
    const posts = posted.filter((m) => m.type === "cycle_mode");
    expect(posts).toHaveLength(1);
    expect(posts[0]).toMatchObject({ tab: 1 });
  });

  it("e. empty tab, BROWSE on the dashboard: exactly one cycle_mode, not two", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "arrive" });
    const root = container.querySelector(".empty-tab")!;
    expect(root.contains(document.activeElement)).toBe(true);
    const event = shiftTab(document.activeElement!);
    expect(event.defaultPrevented).toBe(true);
    const posts = posted.filter((m) => m.type === "cycle_mode");
    expect(posts).toHaveLength(1);
    expect(posts[0]).toMatchObject({ tab: 1 });
  });

  it("f. chooser open, filter input focused: the chooser's own route (cycle_default_mode, active tab live)", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    // No "open" row for tab 1: the chooser's cursor default (`findIndex` over `open`) then lands on
    // "New session", row 0 -- not the "tab" row `cycleMode` flashes locally for -- so this exercises
    // `onCycleMode`/`onChooserCycleMode`, which reads the real active tab (live) off `tabs`, not the
    // chooser's own cursor row.
    dispatch({
      kind: "chooser",
      open: [],
      records: [
        { providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false },
      ],
    });
    const chooser = container.querySelector(".chooser")!;
    fireEvent.keyDown(chooser, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(document.activeElement).toBe(filter);
    // Left empty deliberately: a non-empty needle that matches nothing would drop every row
    // (`chooserRows`'s own doc comment -- "New session" drops out while filtering, and so does an
    // unmatched open/record row), which is a different scenario than this test's own.
    const event = shiftTab(filter);
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(filter);
    expect(filter.value).toBe("");
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(lastOfType("cycle_default_mode")).toBeDefined();
  });

  /** v1 (D6): "a starting tab can switch" -- no separate "starting" flash any more, it simply
   *  cycles like `not_started`/`live`. */
  it("g. a starting tab: posts too, no flash", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    act(() => widen(container));
    const root = container.querySelector(".empty-tab")!;
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });

  it("h. the close prompt open: it closes (tmux confirm-before, any key cancels), no post", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    expect(container.querySelector(".band-prompt")!.textContent).toBe("close 1? (y/n)");
    const root = container.querySelector(".agent-ui-conversation")!;
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(container.querySelector(".band-prompt")).toBeNull();
    expect(lastOfType("close_tab")).toBeUndefined();
    expect(lastOfType("cycle_mode")).toBeUndefined();
    vi.unstubAllGlobals();
  });

  /** v1 (D6): a live tab's `<leader>` `mode.cycle` always posts now too, the same as its Shift+Tab
   *  (test a/b above) -- there is no capability left to grey the box entry out for. */
  it("i. <leader> mode.cycle on a live tab posts, and the box does not grey it out", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    const root = container.querySelector(".agent-ui-conversation")!;
    act(() => (root as HTMLElement).focus());
    vi.useFakeTimers();
    try {
      fireEvent.keyDown(root, { key: " " });
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      const box = container.querySelector(".which-key-box")!;
      expect(box).not.toBeNull();
      expect(box.textContent).toContain("mode");
      expect(box.querySelector(".wk-disabled")).toBeNull();
      fireEvent.keyDown(root, { key: "m" });
    } finally {
      vi.useRealTimers();
    }
    expect(container.querySelector(".band-message")).toBeNull();
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    vi.unstubAllGlobals();
  });
  /** Whole-branch review: R4 names Shift+Tab and `<leader>` `mode.cycle` as one rule, but on a
   *  `starting` or `failed` tab `EmptyTab` returned before its leader engine, so `<leader>m` did
   *  nothing at all; and `runPanelAction` gated on `live || ended`, so reaching it there would have
   *  posted a `cycle_mode` Rust only refuses. Both now follow `modeKeyRoute`. v1 (D6): `failed` in
   *  auto (not already bypass) is still the one state this flashes rather than posts -- and the text
   *  now names `r` (the fixed reset key), not `newTabChord` (there is no rebindable "leave" chord). */
  it("j. <leader> mode.cycle on a failed tab flashes 'the session has ended — r to start again', no post", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: "claude is not on PATH" }] });
    act(() => widen(container));
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    dispatch({ kind: "arrive" });
    const target = (document.activeElement ?? document.body) as HTMLElement;
    expect(container.querySelector(".empty-tab")!.contains(target)).toBe(true);
    fireEvent.keyDown(target, { key: " " });
    fireEvent.keyDown(target, { key: "m" });
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage("r"));
    vi.unstubAllGlobals();
  });

  /** v1 (D6): "a starting tab can switch" -- `<leader>` `mode.cycle` posts on it too now, the same
   *  as Shift+Tab (test g above), replacing wave 5's separate "session is starting" flash. */
  it("j2. <leader> mode.cycle on a starting tab posts too, no flash", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    act(() => widen(container));
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    dispatch({ kind: "arrive" });
    const target = (document.activeElement ?? document.body) as HTMLElement;
    expect(container.querySelector(".empty-tab")!.contains(target)).toBe(true);
    fireEvent.keyDown(target, { key: " " });
    fireEvent.keyDown(target, { key: "m" });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });

  /** v1 (D6): the one case that still refuses to enter bypass -- an already-ended tab that is not
   *  already in bypass -- but that same tab, already in bypass, may still leave it. Both the plain
   *  Shift+Tab route and the leader's `mode.cycle` share `modeKeyRoute`, so one check here stands in
   *  for both call sites; the pure cases (every `tabState`/`tabMode` pair) are `modeKey.test.ts`'s. */
  it("k. an ended tab already in bypass still cycles (to leave it) -- D6", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "ended", mode: "bypass" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState() });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")!;
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });
});

/* K01 (ruling R1: "the empty tab's dashboard follows the same rule"), fix round 1 (review): the
   empty tab keeps its own waiting prefix and leader sequence, which `dropPendingKeys()` did not
   reach. A cancel route the dashboard never sees as a key of its own -- Shift+Tab (stopped by
   `onModeKey`), the chooser, a switch to another empty tab, a bypass prompt -- left them armed, so
   the next key was swallowed (`g`, Shift+Tab, `i` opened no composer) or completed the sequence. */
describe("K01 on the empty tab: every cancel route drops its waiting prefix and leader", () => {
  function emptyInBrowse() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(rendered.container.querySelector(".empty-tab")!.contains(document.activeElement)).toBe(true);
    expect(rendered.container.querySelector("textarea")).toBeNull();
    return rendered;
  }
  const press = (key: string, init: Record<string, unknown> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
  const composer = (container: HTMLElement) => container.querySelector("textarea");

  it("control: with nothing waiting, i opens the composer, and g then i does not", () => {
    const { container } = emptyInBrowse();
    press("g");
    press("i");
    expect(composer(container)).toBeNull();
    press("i");
    expect(composer(container)).not.toBeNull();
  });
  it("g, then Shift+Tab (the mode cycles), then i opens the composer", () => {
    const { container } = emptyInBrowse();
    press("g");
    press("Tab", { shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    press("i");
    expect(composer(container)).not.toBeNull();
  });
  it("Space, then Shift+Tab, then m never reaches <leader>m", () => {
    vi.useFakeTimers();
    try {
      const { container } = emptyInBrowse();
      dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
      act(() => vi.advanceTimersByTime(1000));
      press(" ");
      act(() => vi.advanceTimersByTime(1000));
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      press("Tab", { shiftKey: true });
      expect(container.querySelector(".which-key-box")).toBeNull();
      press("m");
      act(() => vi.advanceTimersByTime(1000));
      expect(posted.filter((m) => m.type === "cycle_mode"), "Shift+Tab's own, and no second").toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
  it.each<[string, () => void]>([
    [
      "the chooser, closed by Esc",
      () => {
        dispatch({ kind: "chooser", open: [], records: [] });
        press("Escape");
      },
    ],
    [
      "a switch to another empty tab",
      () =>
        dispatch({
          kind: "tabs",
          active: 2,
          tabs: [
            { ...LIVE_TAB, state: "not_started" },
            { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" },
          ],
        }),
    ],
    [
      "a bypass prompt, answered n",
      () => {
        dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["Switch to bypass? (y/n)"] });
        press("n");
      },
    ],
  ])("g, then %s, then i opens the composer", (_name, route) => {
    const { container } = emptyInBrowse();
    press("g");
    route();
    expect(container.querySelector(".empty-tab")!.contains(document.activeElement)).toBe(true);
    press("i");
    expect(composer(container)).not.toBeNull();
  });
});

/** v1 (spec `docs/superpowers/specs/2026-09-27-v1-mode-design.md`, D2/D6/D7/D11): entering bypass
 *  asks a y/n first, and only a LONE `y`/`Y` -- not one that lands as part of typed text, or too
 *  soon after the prompt appeared -- may answer it. `bypassYesCounts`'s own arithmetic (every
 *  `now`/`openedAt`/`lastKeyAt` combination) is `modeKey.test.ts`'s; this file wires it through a
 *  real `App` render, a real `confirm_bypass` envelope and real keydowns. */
describe("v1 mode: entering bypass asks first", () => {
  /** Fakes `performance.now()` alongside the timer functions -- plain `vi.useFakeTimers()` leaves
   *  `performance.now()` real (confirmed empirically), and `App.tsx`'s guard reads the clock through
   *  nothing else. */
  function fakeClock() {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  }
  function dispatchBypassConfirm(
    over: Partial<{ tab: number | null; scope: "tab" | "default"; nonce: number; lines: string[] }> = {},
  ) {
    dispatch({
      kind: "confirm_bypass",
      tab: over.tab === undefined ? 1 : over.tab,
      scope: over.scope ?? "tab",
      nonce: over.nonce ?? 7,
      lines: over.lines ?? ["Switch to bypass and approve the 2 waiting cards? (y/n)"],
    });
  }
  /** Mounts `App`, delivers `hello` and a live tab 1 (legacy backend, auto mode -- ordinary
   *  defaults), and returns the conversation root every test answers keys on. */
  function liveConversation() {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    return { container, root: container.querySelector(".agent-ui-conversation")! };
  }

  it("a. live tab Shift+Tab posts cycle_mode whatever capabilities says (legacy included)", () => {
    const { root } = liveConversation();
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    vi.unstubAllGlobals();
  });

  it("b. draws lines[0] in the band; a lone y after the guard posts confirm_bypass with the envelope's own values and closes", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm({ tab: 1, scope: "tab", nonce: 7, lines: ["Switch to bypass and approve the 2 waiting cards? (y/n)"] });
      expect(container.querySelector(".band-prompt")!.textContent).toBe("Switch to bypass and approve the 2 waiting cards? (y/n)");
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "y" });
      expect(lastOfType("confirm_bypass")).toMatchObject({ tab: 1, scope: "tab", nonce: 7 });
      expect(container.querySelector(".band-prompt")).toBeNull();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("b2. Y (capital) does the same as y (P7)", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm({ nonce: 9 });
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "Y" });
      expect(lastOfType("confirm_bypass")).toMatchObject({ nonce: 9 });
      expect(container.querySelector(".band-prompt")).toBeNull();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it.each(["n", "Escape", "q"])("b3. %s closes and posts nothing", (key) => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
      expect(container.querySelector(".band-prompt")).toBeNull();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("b4. a bare Shift does nothing -- the prompt stays open", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "Shift" });
      expect(container.querySelector(".band-prompt")).not.toBeNull();
      expect(lastOfType("confirm_bypass")).toBeUndefined();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it('c. a scope:"default" envelope posts tab: null', () => {
    fakeClock();
    try {
      const { root } = liveConversation();
      dispatchBypassConfirm({ tab: null, scope: "default", nonce: 11, lines: ["Start new sessions in bypass? (y/n)"] });
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "y" });
      expect(lastOfType("confirm_bypass")).toMatchObject({ tab: null, scope: "default", nonce: 11 });
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("d. a second envelope while one is open replaces it (Reprompt): the newest nonce is what gets posted", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm({ nonce: 1, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
      dispatchBypassConfirm({ nonce: 2, lines: ["Switch to bypass and approve the 2 waiting cards? (y/n)"] });
      expect(container.querySelector(".band-prompt")!.textContent).toBe("Switch to bypass and approve the 2 waiting cards? (y/n)");
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "y" });
      expect(lastOfType("confirm_bypass")).toMatchObject({ nonce: 2 });
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("d2. a Reprompt restarts the prompt's own 250ms on-screen wait, rather than inheriting the first envelope's elapsed time", () => {
    fakeClock();
    try {
      const { root } = liveConversation();
      dispatchBypassConfirm({ nonce: 1 });
      act(() => vi.advanceTimersByTime(240));
      dispatchBypassConfirm({ nonce: 2 }); // Reprompt: openedAt resets to now
      // 240ms since the REPROMPT (480ms since the very first envelope, which would clear a
      // guard measured from there) -- still under the guard measured from the reprompt itself.
      act(() => vi.advanceTimersByTime(240));
      fireEvent.keyDown(root, { key: "y" });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("e. while the prompt is open, Shift+Tab does not post a second cycle_mode (modeKeyRoute -> overlay)", () => {
    const { root } = liveConversation();
    dispatchBypassConfirm();
    posted.length = 0; // only what happens AFTER the prompt opens counts here
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(0);
    vi.unstubAllGlobals();
  });

  it("f. <leader> mode.cycle on a live tab posts cycle_mode, and the which-key box does not grey it out", () => {
    const { container, root } = liveConversation();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    act(() => (root as HTMLElement).focus());
    vi.useFakeTimers();
    try {
      fireEvent.keyDown(root, { key: " " });
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      const box = container.querySelector(".which-key-box")!;
      expect(box.querySelector(".wk-disabled")).toBeNull();
      fireEvent.keyDown(root, { key: "m" });
    } finally {
      vi.useRealTimers();
    }
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
    vi.unstubAllGlobals();
  });

  /** D11: typed text can never answer the prompt, even when the very keys that triggered it (here,
   *  the leader's own `Space m`) are what land right before the `y`. A live tab in BROWSE with a
   *  card already waiting -- the realistic case R06's own wording describes. */
  it("g. Space, m, y at 100ms with the envelope arriving between m and y: no confirm_bypass, no permission_response, the band names why", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
      dispatch({
        kind: "events",
        tab: 1,
        fromRevision: 0,
        throughRevision: 2,
        events: [
          { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: {} },
          { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
        ],
      });
      act(() => (root as HTMLElement).focus());
      fireEvent.keyDown(root, { key: " " });
      act(() => vi.advanceTimersByTime(100));
      fireEvent.keyDown(root, { key: "m" });
      dispatchBypassConfirm({ lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
      act(() => vi.advanceTimersByTime(100));
      fireEvent.keyDown(root, { key: "y" });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
      expect(lastOfType("permission_response")).toBeUndefined();
      expect(container.querySelector(".band-prompt")).toBeNull();
      expect(container.querySelector(".band-message")!.textContent).toBe(
        "y must be pressed on its own to enter bypass — Shift+Tab to ask again",
      );
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** D11's two named halves of `bypassYesCounts`, exercised end to end (every raw `now`/`openedAt`/
   *  `lastKeyAt` combination, including the boundary, is `modeKey.test.ts`'s own `bypassYesCounts`
   *  describe block -- these two just prove `App.tsx` actually wires the guard through). A key that
   *  lands WHILE the prompt is already open cancels it outright via D1's own "any key but a counted
   *  y/Y" rule (test b3) before the guard's stand-alone half would even matter -- and a key close
   *  enough before the ANSWER to trip the stand-alone half is necessarily close enough to when the
   *  prompt opened to also trip the on-screen half (the prompt cannot have been open for the guard's
   *  own duration AND have a non-cancelling key land within the guard's duration of it, at once).
   *  So test h2 below reproduces both halves failing together, the same shape as test g. */
  it.each(["y", "Y"])("h. %s cancelled: too soon after the prompt opened, no other key at all (on-screen rule)", (key) => {
    fakeClock();
    try {
      const { root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(100));
      fireEvent.keyDown(root, { key });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it.each(["y", "Y"])("h2. %s cancelled: another key landed shortly before it, before the prompt even opened", (key) => {
    fakeClock();
    try {
      const { root } = liveConversation();
      act(() => (root as HTMLElement).focus());
      fireEvent.keyDown(root, { key: "x" });
      act(() => vi.advanceTimersByTime(100));
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(100)); // 200ms since "x", 100ms since the prompt opened
      fireEvent.keyDown(root, { key });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** Whole-branch review (codex, 2026-09-28): a `y` with Ctrl/Alt/Meta held, or one an input method
   *  is composing, is an editing keystroke, not D11's "y pressed on its own" -- the timing guard
   *  alone let `Ctrl+y` (or the first letter of a pinyin syllable) enter bypass and approve the
   *  waiting cards once the prompt had been up for the guard's duration. Each cancels, like any key
   *  but a counted y. Shift is not in the list: `Y` is a counted answer (test b2).
   *
   *  Super/Hyper (v1 audit fixes, 2026-09-28): 325e007's own fix never checked
   *  `getModifierState("Super"/"Hyper")` -- reconciling it with this branch's R2 ("refuse any Alt,
   *  Meta or Super", `keymap.ts`'s `isPlainAnswerKey`) found that gap and this closes it. `jsdom`
   *  honours `KeyboardEventInit`'s `modifierSuper` and `modifierHyper` as two independent booleans
   *  (`EventModifierMixin-impl.js`'s `getModifierState` reads `this.modifier${keyArg}` for either),
   *  so `Super+y` and `Hyper+y` below each isolate one without implying the other, the way a
   *  physical keypress would report it (see keymap.ts's `KeyLike` doc comment on the honest caveat
   *  about whether WebKitGTK ever surfaces either as a DOM modifier at all).
   *
   *  AltGraph (fix round 2, v1 audit review, "the AltGraph clause"): a level-3 shift some layouts
   *  use to type an ordinary character -- neither 325e007 nor R2 named it, and `isPlainAnswerKey`
   *  had no check for it until this round; `jsdom` honours `modifierAltGraph` the same way it
   *  honours `modifierSuper`. */
  it.each([
    ["Ctrl+y", { key: "y", ctrlKey: true }],
    ["Alt+y", { key: "y", altKey: true }],
    ["Meta+y", { key: "y", metaKey: true }],
    ["Super+y", { key: "y", modifierSuper: true }],
    ["Hyper+y", { key: "y", modifierHyper: true }],
    ["AltGraph+y", { key: "y", modifierAltGraph: true }],
    ["Ctrl+Shift+Y", { key: "Y", ctrlKey: true, shiftKey: true }],
    ["a y an input method is composing", { key: "y", isComposing: true }],
    ["an input method's keyCode 229 y", { key: "y", keyCode: 229 }],
  ] as const)("h3. %s after the guard cancels and posts nothing", (_name, init) => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, init);
      expect(lastOfType("confirm_bypass")).toBeUndefined();
      expect(container.querySelector(".band-prompt")).toBeNull();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /** Fix round 2 (v1 audit review, "the tests pin behaviour, not the use of the shared predicate"):
   *  h3 above pins every modifier/composing case behaviourally, but a same-shaped hand-written copy
   *  substituted for the `isPlainAnswerKey` call in the bypass-y handler would pass every one of
   *  them too -- it only proves the RESULT, not that the two surfaces genuinely share one rule the
   *  way the reconciliation commit (9b9c65a) claims. This spies on the real, exported
   *  `keymap.ts#isPlainAnswerKey` (imported as a namespace, so the spy replaces the same binding
   *  `App.tsx`'s own `import { isPlainAnswerKey }` resolves through -- confirmed in a throwaway
   *  cross-module spike before writing this) and catches the one mutation at this call site h3
   *  cannot: `App.tsx` reverting to a same-shaped hand-written modifier list (the spy sees zero
   *  calls; confirmed by mutation -- every h3 row still passes, only this test turns red, exactly as
   *  efacbb1's own commit message says). Hardcoding `plain = true` is a DIFFERENT mutation and not
   *  that gap: confirmed by mutation, it fails every h3 row (all nine) as well as this one -- h3's
   *  own behavioural assertions already catch it, so this test is merely redundant on that
   *  particular case, not the reason it exists. It cannot reach `keymap.ts`'s OWN two internal call
   *  sites (the `a`/`d` case, the `Shift+D` arm) -- those are intra-module references the bundler
   *  compiles to a direct local call, not a property read through the spied namespace object,
   *  confirmed the same way. `keymap.test.ts`'s own matrix test (v1 audit fixes, finding 1) is what
   *  pins those two now, not behavioural tests alone -- though even that matrix cannot reach a bug
   *  inside `isPlainAnswerKey` itself, since it derives its own expectation by calling the same
   *  function under test; a direct `isPlainAnswerKey` unit test (and this file's own Hyper-alone h3
   *  row) covers that instead. */
  it("h3b. the bypass-y handler calls the real, exported isPlainAnswerKey, not a look-alike copy", () => {
    const spy = vi.spyOn(keymapModule, "isPlainAnswerKey");
    fakeClock();
    try {
      const { root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(300));
      spy.mockClear();
      fireEvent.keyDown(root, { key: "y", ctrlKey: true });
      expect(spy).toHaveBeenCalledTimes(1);
      expect(lastOfType("confirm_bypass")).toBeUndefined();
    } finally {
      spy.mockRestore();
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  /* The sandbox pass (2026-09-28, the bypass follow-up): WebKitGTK names the Super key "Super",
     which the prompt's old four-name modifier list did not know, so holding Super closed the prompt
     by itself and the y after it copied a row instead. Super's own keydown is a modifier now, and a
     y while it is held is refused like any other modified y. */
  it("h4. Super's own keydown keeps the prompt, and a y while Super is held is refused with the flash", () => {
    fakeClock();
    try {
      const { container, root } = liveConversation();
      dispatchBypassConfirm();
      act(() => vi.advanceTimersByTime(300));
      fireEvent.keyDown(root, { key: "Super", code: "OSLeft" });
      expect(container.querySelector(".band-prompt")).not.toBeNull();
      fireEvent.keyDown(root, { key: "y", code: "KeyY" });
      expect(lastOfType("confirm_bypass")).toBeUndefined();
      expect(container.querySelector(".band-prompt")).toBeNull();
      expect(container.textContent).toContain("y must be pressed on its own");
      fireEvent.keyUp(root, { key: "Super", code: "OSLeft" });
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  describe("i. every route away cancels the prompt (spec §3.4)", () => {
    it.each([
      ["pane_focus losing focus", () => dispatch({ kind: "pane_focus", focused: false })],
      ["a tabs envelope naming a DIFFERENT active tab", () => dispatch({ kind: "tabs", active: 2, tabs: [{ ...LIVE_TAB, id: 2 }] })],
      ["arrive", () => dispatch({ kind: "arrive" })],
      ["focus_permission", () => dispatch({ kind: "focus_permission", tab: 1 })],
      ["enter_input", () => dispatch({ kind: "enter_input" })],
      ["chooser", () => dispatch({ kind: "chooser", open: [], records: [] })],
      // Fix round 1 (whole-branch review, blocking): GTK claims these keys before the WebView sees
      // a keydown, so none of them passes through `answerConfirm` -- and "HINT then y" (copy a code
      // block) or a rename starting with y answered the prompt.
      ["hint_collect (prefix f)", () => dispatch({ kind: "hint_collect", sessionId: 1 })],
      ["begin_rename (prefix ,)", () => dispatch({ kind: "begin_rename", tab: 1, current: null })],
      ["tab_detail (prefix i), whose own y copies a row", () => dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "cwd", value: "/p" }] })],
      ["open_keymap (prefix ?)", () => dispatch({ kind: "open_keymap" })],
      ["nav_key (Ctrl+j)", () => dispatch({ kind: "nav_key", direction: "down" })],
      ["literal_key (prefix C-b)", () => dispatch({ kind: "literal_key", key: "C-a" })],
    ] as const)("%s cancels it, and a y 300ms later posts nothing", (_name, cancel) => {
      fakeClock();
      try {
        const { container } = liveConversation();
        dispatchBypassConfirm();
        expect(container.querySelector(".band-prompt")).not.toBeNull();
        cancel();
        expect(container.querySelector(".band-prompt")).toBeNull();
        act(() => vi.advanceTimersByTime(300));
        const root = container.querySelector(".agent-ui-conversation") ?? container.querySelector(".empty-tab")!;
        fireEvent.keyDown(root, { key: "y" });
        expect(lastOfType("confirm_bypass")).toBeUndefined();
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });

    it("a tabs envelope naming the SAME active tab does not cancel it", () => {
      fakeClock();
      try {
        const { container } = liveConversation();
        dispatchBypassConfirm();
        dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
        expect(container.querySelector(".band-prompt")).not.toBeNull();
        act(() => vi.advanceTimersByTime(300));
        fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "y" });
        expect(lastOfType("confirm_bypass")).toBeDefined();
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });
  });

  describe("j. the chooser (spec §3.4)", () => {
    it("over an empty tab: Shift+Tab posts cycle_mode; the envelope shows over the chooser without closing it; y after the guard posts", () => {
      fakeClock();
      try {
        const widen = stubBandWidth();
        const { container } = render(<App />);
        dispatch({ kind: "hello", ...HELLO });
        dispatchEmptyTab();
        act(() => widen(container));
        dispatch({ kind: "chooser", open: [], records: [] });
        const chooser = container.querySelector(".chooser")!;
        fireEvent.keyDown(chooser, { key: "Tab", shiftKey: true }); // cursor starts on "New session"
        expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(1);
        dispatchBypassConfirm({ nonce: 4 });
        expect(container.querySelector(".chooser")).not.toBeNull();
        expect(container.querySelector(".band-prompt")).not.toBeNull();
        act(() => vi.advanceTimersByTime(300));
        fireEvent.keyDown(chooser, { key: "y" });
        expect(lastOfType("confirm_bypass")).toMatchObject({ nonce: 4 });
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });

    it("over a live tab: Shift+Tab posts cycle_default_mode instead", () => {
      fakeClock();
      try {
        const { container } = liveConversation();
        dispatch({
          kind: "chooser",
          open: [],
          records: [{ providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false }],
        });
        const chooser = container.querySelector(".chooser")!;
        fireEvent.keyDown(chooser, { key: "Tab", shiftKey: true }); // cursor on "New session" (no "open" row for tab 1)
        expect(lastOfType("cycle_mode")).toBeUndefined();
        expect(lastOfType("cycle_default_mode")).toBeDefined();
        dispatchBypassConfirm({ tab: null, scope: "default", nonce: 5 });
        act(() => vi.advanceTimersByTime(300));
        fireEvent.keyDown(chooser, { key: "y" });
        expect(lastOfType("confirm_bypass")).toMatchObject({ tab: null, scope: "default", nonce: 5 });
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });

    it("with the prompt open, Enter cancels it and picks no row; j/k do not move the chooser's selection", () => {
      fakeClock();
      try {
        const widen = stubBandWidth();
        const { container } = render(<App />);
        dispatch({ kind: "hello", ...HELLO });
        dispatchEmptyTab();
        act(() => widen(container));
        // Two records, so from the first record both `j` and `k` WOULD move the cursor if they reached
        // the chooser -- a row where one of them is a no-op anyway proves nothing about that key.
        dispatch({
          kind: "chooser",
          open: [],
          records: [
            { providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false },
            { providerSessionId: "free-1111", name: null, title: "free two", createdAt: "1", updatedAt: "2", heldElsewhere: false },
          ],
        });
        const chooser = container.querySelector(".chooser")!;
        const current = () => container.querySelector(".chooser-row.current")!.textContent;
        fireEvent.keyDown(chooser, { key: "j" }); // no prompt yet: onto "free one", the middle row
        const before = current();
        expect(before).toContain("free one");
        dispatchBypassConfirm();
        expect(container.querySelector(".band-prompt")).not.toBeNull();
        fireEvent.keyDown(chooser, { key: "j" });
        expect(container.querySelector(".band-prompt")).toBeNull(); // j answered the prompt (cancel)...
        expect(current()).toBe(before); // ...and did nothing else
        // `k` needs a prompt of its own: the `j` above already closed the first one, so a `k` sent now
        // would reach the chooser with no prompt open and prove nothing (fix round 1, item B).
        dispatchBypassConfirm({ nonce: 8 });
        act(() => vi.advanceTimersByTime(300));
        expect(container.querySelector(".band-prompt")).not.toBeNull();
        fireEvent.keyDown(chooser, { key: "k" });
        expect(container.querySelector(".band-prompt")).toBeNull();
        expect(current()).toBe(before);
        // A fresh prompt, then Enter: cancels, picks nothing, closes -- "nothing but the cancel happens".
        dispatchBypassConfirm({ nonce: 20 });
        fireEvent.keyDown(chooser, { key: "Enter" });
        expect(container.querySelector(".band-prompt")).toBeNull();
        expect(current()).toBe(before);
        expect(lastOfType("resume")).toBeUndefined();
        expect(lastOfType("confirm_bypass")).toBeUndefined();
      } finally {
        vi.useRealTimers();
        vi.unstubAllGlobals();
      }
    });
  });
});

/** v1-mode fix round 1: Codex finding 5 (the chooser let a key it does not handle reach the card
 *  under it) and the whole-branch review's panel findings -- the typing guard never saw the key that
 *  answered a prompt, a held `y` counted, the chooser's `y` was handled twice, and the chooser's own
 *  inputs kept Enter/Escape from the prompt. */
describe("v1 mode fix round 1: the prompt and the chooser take the keys whole", () => {
  function fakeClock() {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  }
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });
  const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
  const press = (key: string, init: Record<string, unknown> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
  const answered = () => posted.filter((m) => m.type === "permission_response");
  const confirms = () => posted.filter((m) => m.type === "confirm_bypass");

  /** One Write card on a live tab, the keys arrived on it (`arrive` lands on the oldest card). */
  function arrivedOnACard() {
    const widen = stubBandWidth();
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    const list: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Write", input: { file_path: "a.txt", content: "x" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Write", input: { file_path: "a.txt", content: "x" } },
    ];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    expect(rendered.container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    return rendered;
  }

  it.each(["a", "d"])("Codex 5: the chooser open over a card -- a lone %s answers nothing and the chooser stays", (key) => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "chooser", open: [], records: [] });
    const chooser = container.querySelector(".chooser")!;
    expect(document.activeElement).toBe(chooser);
    wait(300);
    press(key);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(container.querySelector(".chooser")).not.toBeNull();
  });

  it("the key that cancels the prompt still counts for the typing guard: x then a 100 ms later answers nothing", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 3, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
    wait(1000);
    press("x");
    expect(container.querySelector(".band-prompt")).toBeNull();
    wait(100);
    press("a");
    wait(1000);
    expect(answered()).toEqual([]);
    expect(confirms()).toEqual([]);
  });

  it("a held y's autorepeat never answers the reprompt that followed its first press", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
    wait(300);
    press("y");
    expect(confirms().map((m) => m.nonce)).toEqual([1]);
    wait(20);
    // D7: a card arrived while the prompt was up, so Rust answers with a fresh prompt.
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 2, lines: ["Switch to bypass and approve the 2 waiting cards? (y/n)"] });
    wait(280);
    press("y", { repeat: true });
    expect(confirms().map((m) => m.nonce)).toEqual([1]);
    expect(container.querySelector(".band-prompt")).toBeNull();
  });

  /** Defect 2 (2026-09-27 sandbox GUI pass): distinct from the test just above, which holds `y`
   *  across a D7 REPROMPT. Here nothing reprompts -- the FIRST `y` (too soon after the prompt
   *  opened, so it fails the guard's on-screen half) just cancels, leaving `confirm` null, and the
   *  physical key is still held. Before the fix, each auto-repeat after that fell through to
   *  BROWSE's own `y` (copy the row under the cursor), which calls `copied()` and overwrites the
   *  D11 flash with "copied N chars" within about one repeat interval. */
  it("defect 2: after a held y cancels the prompt, its autorepeats do not fall through as BROWSE's copy", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
    // Too soon after the prompt opened (needs >= 250ms): this first, non-repeat "y" cancels rather
    // than answers -- the physical first keydown of a held press is never itself a repeat.
    wait(100);
    press("y");
    expect(confirms()).toEqual([]);
    expect(container.querySelector(".band-message")!.textContent).toBe(
      "y must be pressed on its own to enter bypass — Shift+Tab to ask again",
    );
    // The OS keeps sending the held key's auto-repeat. None of these may answer (kept, per the test
    // above) NOR fall through as an ordinary BROWSE key that stomps the explanation just shown.
    press("y", { repeat: true });
    press("y", { repeat: true });
    expect(confirms()).toEqual([]);
    expect(container.querySelector(".band-message")!.textContent).toBe(
      "y must be pressed on its own to enter bypass — Shift+Tab to ask again",
    );
  });

  it("defect 2: once the held key is released, a fresh y answers the next prompt normally", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
    wait(100);
    press("y"); // cancels
    press("y", { repeat: true }); // swallowed
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "y" });
    wait(500);
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 2, lines: ["Switch to bypass and approve the 1 waiting card? (y/n)"] });
    wait(300);
    press("y"); // a genuinely fresh, non-repeat press
    expect(confirms().map((m) => m.nonce)).toEqual([2]);
    expect(container.querySelector(".band-prompt")).toBeNull();
  });

  it("the chooser's y over a live tab posts confirm_bypass exactly once", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "chooser", open: [], records: [] });
    dispatch({ kind: "confirm_bypass", tab: null, scope: "default", nonce: 5, lines: ["Start new sessions in bypass? (y/n)"] });
    wait(300);
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "y" });
    expect(confirms()).toEqual([expect.objectContaining({ scope: "default", nonce: 5 })]);
  });

  it("Enter in the chooser's filter with the prompt open only cancels the prompt: the filter stays and nothing is chosen", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({
      kind: "chooser",
      open: [],
      records: [{ providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false }],
    });
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(document.activeElement).toBe(filter);
    dispatch({ kind: "confirm_bypass", tab: null, scope: "default", nonce: 6, lines: ["Start new sessions in bypass? (y/n)"] });
    wait(300);
    fireEvent.keyDown(filter, { key: "Enter" });
    expect(container.querySelector(".band-prompt")).toBeNull();
    expect(container.querySelector(".chooser-filter")).not.toBeNull();
    expect(lastOfType("resume")).toBeUndefined();
    expect(lastOfType("tab_verb")).toBeUndefined();
    expect(confirms()).toEqual([]);
  });

  it("Escape in the chooser's rename field with the prompt open only cancels the prompt: the rename stays open", () => {
    fakeClock();
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    dispatch({ kind: "chooser", open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: false }], records: [] });
    // The cursor starts on the active tab's row; Ctrl+r opens its rename.
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "r", ctrlKey: true });
    const rename = container.querySelector<HTMLInputElement>(".chooser-rename")!;
    expect(document.activeElement).toBe(rename);
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["Switch to bypass? (y/n)"] });
    wait(300);
    fireEvent.keyDown(rename, { key: "Escape" });
    expect(container.querySelector(".band-prompt")).toBeNull();
    expect(container.querySelector(".chooser-rename")).not.toBeNull();
    expect(confirms()).toEqual([]);
  });

  /** Shift+Tab inside the tab bar's rename field is still the mode key (the document-capture router
   *  has no rename overlay to defer to), so a bypass prompt can open over it. */
  it("a bypass prompt open over the tab bar's rename takes its Enter: the prompt cancels, the rename is not committed", () => {
    fakeClock();
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    dispatch({ kind: "begin_rename", tab: 1, current: null });
    const rename = container.querySelector<HTMLInputElement>(".tab-rename")!;
    expect(document.activeElement).toBe(rename);
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 9, lines: ["Switch to bypass? (y/n)"] });
    wait(300);
    fireEvent.keyDown(rename, { key: "Enter" });
    expect(container.querySelector(".band-prompt")).toBeNull();
    expect(container.querySelector(".tab-rename")).not.toBeNull();
    expect(lastOfType("rename_tab")).toBeUndefined();
    expect(confirms()).toEqual([]);
  });

  it("Enter in the composer with the prompt open only cancels the prompt: nothing is sent", () => {
    fakeClock();
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    enterInputMode(container);
    const box = container.querySelector<HTMLTextAreaElement>(".composer textarea")!;
    fireEvent.change(box, { target: { value: "hello" } });
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 10, lines: ["Switch to bypass? (y/n)"] });
    wait(300);
    fireEvent.keyDown(box, { key: "Enter" });
    expect(container.querySelector(".band-prompt")).toBeNull();
    expect(lastOfType("send_message")).toBeUndefined();
    expect(lastOfType("queue_message")).toBeUndefined();
    expect(confirms()).toEqual([]);
  });
});

/* Task 3 (v1 v1-ui plan): the page's own half of C1's mechanism (spec §3.5) -- the `panel_keys`
   mirror this page posts, and the `nav_key`/`nav_fallthrough` round trip a claimed `Ctrl+j`/`Ctrl+k`
   takes. Task 2 (Rust) is a sibling task; this file only ever posts/dispatches the wire envelopes
   the plan's own "Interfaces" block fixes, never anything from `install_module_nav` itself. */
describe("v1 C1: the composer mirror (spec §3.5)", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    // The `a.rs`/`b.rs` transcript line is only exercised by the "gf pick" row below; harmless to
    // every other case, which never presses `g` then `f`.
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "compare a.rs and b.rs" }] }), 0);
    return rendered;
  }
  function conversationRoot(container: HTMLElement): HTMLElement {
    return container.querySelector(".agent-ui-conversation")!;
  }
  function closeSession(container: HTMLElement) {
    void container;
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events: [{ type: "session_closed", reason: "provider exited" }] });
  }

  it("posts browse while the live conversation is in BROWSE", () => {
    startedApp();
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "browse" });
  });

  it("posts input once the composer is entered", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "i" });
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "input" });
  });

  // Fix round 1 (reviewer finding): the title used to say "five mirrors" while the assertion below
  // checks four -- `before` is captured already sitting in the first (BROWSE) of the five states, so
  // only the four transitions AWAY from it post; the title now says what the assertion actually
  // checks.
  it("posts only on change: BROWSE -> INPUT -> BROWSE -> INPUT -> BROWSE is four new mirrors, not five", () => {
    const { container } = startedApp();
    const before = posted.filter((m) => m.type === "panel_keys").length;
    const root = conversationRoot(container);
    fireEvent.keyDown(root, { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    fireEvent.keyDown(root, { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    const mirrors = posted.filter((m) => m.type === "panel_keys").slice(before);
    expect(mirrors.map((m) => m.mode)).toEqual(["input", "browse", "input", "browse"]);
  });

  it.each<[string, (container: HTMLElement) => void]>([
    ["? (the keymap overlay)", (container) => fireEvent.keyDown(conversationRoot(container), { key: "?" })],
    ["chooser", (container) => {
      void container;
      dispatch({ kind: "chooser", open: [], records: [] });
    }],
    ["details", (container) => {
      void container;
      dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "account", value: "work" }] });
    }],
    ["rename", (container) => {
      void container;
      dispatch({ kind: "begin_rename", tab: 1, current: null });
    }],
    ["/ (search)", (container) => fireEvent.keyDown(conversationRoot(container), { key: "/" })],
    ["a y/n", (container) => {
      void container;
      dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    }],
    ["a gf pick", (container) => {
      const root = conversationRoot(container);
      fireEvent.keyDown(root, { key: "g" });
      fireEvent.keyDown(root, { key: "f" });
    }],
  ])("posts other with %s open", (_name, openOverlay) => {
    const { container } = startedApp();
    openOverlay(container);
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "other" });
  });

  it("posts other once the session has ended", () => {
    const { container } = startedApp();
    closeSession(container);
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "other" });
  });

  // Fix round 1 (reviewer finding): before the first `tabs` envelope, `activeTab` is `null` and the
  // render shows only "Connecting to the shell…" -- there is no `EmptyTab`, so no box of any kind.
  // `emptyMode`'s "input" default must not leak into the mirror here (it used to, claiming `Ctrl+k`
  // for a composer that had never mounted).
  it("posts other before the first tabs envelope (no box exists yet)", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    expect(lastOfType("panel_keys")).toMatchObject({ mode: "other" });
  });
});

describe("v1 C1: nav_key and its fallthrough (spec §3.5, Review Focus 2)", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0);
    return rendered;
  }
  function conversationRoot(container: HTMLElement): HTMLElement {
    return container.querySelector(".agent-ui-conversation")!;
  }

  it("nav_key down in BROWSE enters INPUT, no fallthrough", () => {
    const { container } = startedApp();
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "down" });
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(posted.slice(before).some((m) => m.type === "nav_fallthrough")).toBe(false);
  });

  it("nav_key up in INPUT leaves it, cursor unchanged, draft kept, no fallthrough", () => {
    // Fix round 1 (reviewer finding): a transcript with two rows, so "cursor unchanged" is actually
    // checkable -- the earlier version of this test had nothing to assert it against.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 0);
    const root = conversationRoot(container);
    const cursorRowBefore = container.querySelector(".row-current")!.textContent;
    fireEvent.keyDown(root, { key: "i" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "half a thought" } });
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "up" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-draft")!.textContent).toBe("half a thought");
    expect(posted.slice(before).some((m) => m.type === "nav_fallthrough")).toBe(false);
    expect(container.querySelector(".row-current")!.textContent).toBe(cursorRowBefore);
    // Fix round 1 (reviewer finding): the panel root, not `document.body`, must hold real DOM focus
    // afterwards -- the same regression class "returns real DOM focus to the panel root after
    // Escape" above guards against, but this chord had no assertion of its own.
    expect(document.activeElement).toBe(root);
  });

  // Fix round 1 (reviewer finding): `nav_key down` must carry the same caret rule `i`/`o` do --
  // "kept", spec §3.1's own words for this chord -- rather than whatever `composerCaret` was last
  // left at by an unrelated `A` press (a bare `setMode("input")` reads that stale state, since
  // `Composer`'s own effect keys on `mode` alone and does not require a fresh `composerFocusRequest`
  // to run). Reproduced without this fix: the caret below landed at the end of the draft, not at 3.
  it("nav_key down places the caret where it was left, not wherever an earlier A left composerCaret", () => {
    const { container } = startedApp();
    const root = conversationRoot(container);
    fireEvent.keyDown(root, { key: "A", shiftKey: true }); // composerCaret -> "end", once.
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    // Enter INPUT via nav_key down (never i/o/A again), leave the caret at 3, leave via nav_key up --
    // neither of those two touches composerCaret either.
    dispatch({ kind: "nav_key", direction: "down" });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "hello world" } });
    box.setSelectionRange(3, 3);
    fireEvent.keyUp(box, { key: "ArrowLeft" });
    dispatch({ kind: "nav_key", direction: "up" });
    expect(container.querySelector("textarea")).toBeNull();
    dispatch({ kind: "nav_key", direction: "down" });
    const box2 = container.querySelector("textarea")!;
    expect(box2.selectionStart).toBe(3);
    expect(box2.selectionEnd).toBe(3);
  });

  // Whole-branch review of v1-ui: GTK takes `Ctrl+j`/`Ctrl+k` before the WebView sees a keydown, so
  // a claimed `nav_key` is a key pressed in between -- exactly why `pane_focus`/`arrive` cancel a
  // pending `g`/`z`/`[`/`]` prefix or leader sequence. Without that here, `g`, Ctrl+j, a draft,
  // Ctrl+k left the box drawn over the composer and one later `g` completed the stale `gg`.
  it("a claimed nav_key cancels a pending g prefix and hides its box", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 0);
      const root = conversationRoot(container);
      act(() => root.focus());
      const lastRow = container.querySelector(".row-current")!.textContent;
      expect(lastRow).toContain("second");
      fireEvent.keyDown(root, { key: "g" });
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      dispatch({ kind: "nav_key", direction: "down" });
      expect(container.querySelector("textarea")).not.toBeNull();
      expect(container.querySelector(".which-key-box")).toBeNull();
      fireEvent.change(container.querySelector("textarea")!, { target: { value: "a draft" } });
      act(() => vi.advanceTimersByTime(3000));
      dispatch({ kind: "nav_key", direction: "up" });
      expect(container.querySelector("textarea")).toBeNull();
      expect(container.querySelector(".which-key-box")).toBeNull();
      fireEvent.keyDown(root, { key: "g" });
      expect(container.querySelector(".row-current")!.textContent).toBe(lastRow);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a claimed nav_key cancels a pending leader sequence", () => {
    vi.useFakeTimers();
    try {
      const { container } = startedApp();
      dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
      const root = conversationRoot(container);
      act(() => root.focus());
      posted.length = 0;
      fireEvent.keyDown(root, { key: " " });
      fireEvent.keyDown(root, { key: "b" });
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box")).not.toBeNull();
      dispatch({ kind: "nav_key", direction: "down" });
      expect(container.querySelector(".which-key-box")).toBeNull();
      dispatch({ kind: "nav_key", direction: "up" });
      fireEvent.keyDown(root, { key: "d" });
      expect(posted.filter((m) => m.type === "tab_verb")).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("nav_key down while already INPUT (stale) falls through once, no mode change", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "i" });
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "down" });
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(posted.slice(before)).toEqual([{ type: "nav_fallthrough", request_id: expect.any(String), direction: "down" }]);
  });

  it("nav_key down on an ended session falls through", () => {
    const { container } = startedApp();
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: 1, events: [{ type: "session_closed", reason: "provider exited" }] });
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "down" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(posted.slice(before)).toEqual([{ type: "nav_fallthrough", request_id: expect.any(String), direction: "down" }]);
  });

  it("nav_key with an overlay (?) open falls through, and the overlay keeps the keys", () => {
    const { container } = startedApp();
    fireEvent.keyDown(conversationRoot(container), { key: "?" });
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "down" });
    expect(posted.slice(before)).toEqual([{ type: "nav_fallthrough", request_id: expect.any(String), direction: "down" }]);
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
  });

  // Fix round 1 (reviewer finding): before the first `tabs` envelope, `EmptyTab` is not mounted yet
  // (only "Connecting to the shell…" is on screen) -- a `nav_key` here used to be queued into
  // `emptyNavKey` and then silently dropped the moment `EmptyTab` DID mount, because its
  // `navKeySeenRef` seeds itself from that already-stale prop at mount and never fires for it.
  it("nav_key before the first tabs envelope falls through, not silently dropped once EmptyTab later mounts", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    // The "Connecting…" placeholder shares `EmptyTab`'s own `.empty-tab` class name; `.connecting`
    // (only the placeholder has one) is what actually distinguishes "not mounted yet".
    expect(container.querySelector(".connecting")).not.toBeNull();
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "up" });
    expect(posted.slice(before)).toEqual([{ type: "nav_fallthrough", request_id: expect.any(String), direction: "up" }]);
    // A later, real arrival must still work normally: the early fallthrough left no stale request
    // behind for `EmptyTab`'s own `navKeySeenRef` to seed itself from and swallow.
    dispatchEmptyTab();
    expect(container.querySelector("textarea")).not.toBeNull(); // launches INPUT, as it always has.
    dispatch({ kind: "nav_key", direction: "up" });
    expect(container.querySelector("textarea")).toBeNull();
  });
});

describe("v1 C1: nav_key over the empty tab (spec §3.1, §3.5)", () => {
  it("menu nav_key down opens its composer", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch({ kind: "arrive" }); // lands the dashboard's own menu (BROWSE), not the composer.
    expect(container.querySelector("textarea")).toBeNull();
    dispatch({ kind: "nav_key", direction: "down" });
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(posted.some((m) => m.type === "nav_fallthrough")).toBe(false);
  });

  it("composer nav_key up goes back to the menu, root focused", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab(); // starts in the composer (INPUT), as a plain launch always has.
    expect(container.querySelector("textarea")).not.toBeNull();
    dispatch({ kind: "nav_key", direction: "up" });
    expect(container.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector(".empty-tab"));
    expect(posted.some((m) => m.type === "nav_fallthrough")).toBe(false);
  });

  it("a starting tab falls through, never transitioning mode", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    const before = posted.length;
    dispatch({ kind: "nav_key", direction: "down" });
    expect(posted.slice(before)).toEqual([{ type: "nav_fallthrough", request_id: expect.any(String), direction: "down" }]);
    expect(container.textContent).toContain("Starting the agent backend");
  });
});

/* Task 4 (v1 v1-ui plan): o/A and the caret (C1a, spec §3.2; the BROWSE placeholder hint itself,
   C1b/S3, is unit-tested in Composer.test.tsx and not repeated here), j on the last stop (C1c, spec
   §3.4), and Esc while a turn runs (R34, spec §4.2) -- each exercised through a real keydown on the
   conversation root, confirming `resolveKey`'s action actually reaches `Composer`/the band and not
   only that the pure table returns the right shape (`keymap.test.ts` already pins that). The
   chooser's own Esc-over-an-empty-tab landing (R16) is pinned in "the session chooser" above; this
   adds only its live-tab counterpart, for symmetry. */
describe("v1 C1abc: o/A and the caret, j at the end, Esc while running", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "hello" }] }), 0);
    return rendered;
  }
  function conversationRoot(container: HTMLElement): HTMLElement {
    return container.querySelector(".agent-ui-conversation")!;
  }
  /** Types "hello world" into the composer, leaves the caret at 3, then returns to BROWSE -- the
   *  shared setup for both of `o`'s and `A`'s own caret checks below. */
  function typeAndLeaveCaretAt3(container: HTMLElement, root: HTMLElement) {
    fireEvent.keyDown(root, { key: "i" });
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello world" } });
    textarea.setSelectionRange(3, 3);
    fireEvent.keyUp(textarea, { key: "ArrowLeft" });
    fireEvent.keyDown(textarea, { key: "Escape" });
  }

  it("o restores the caret left at 3 in 'hello world'", () => {
    const { container } = startedApp();
    const root = conversationRoot(container);
    typeAndLeaveCaretAt3(container, root);
    fireEvent.keyDown(root, { key: "o" });
    const textarea = container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    expect(document.activeElement).toBe(textarea);
    expect(textarea!.selectionStart).toBe(3);
  });

  it("A puts the caret at the end (11), regardless of where it was left", () => {
    const { container } = startedApp();
    const root = conversationRoot(container);
    typeAndLeaveCaretAt3(container, root);
    fireEvent.keyDown(root, { key: "A", shiftKey: true });
    const textarea = container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    expect(textarea!.selectionStart).toBe("hello world".length);
  });

  it("o and A are refused on an ended session; O and I stay unbound", () => {
    const { container } = startedApp();
    const root = conversationRoot(container);
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 0,
      throughRevision: 1,
      events: [{ type: "session_closed", reason: "provider exited" }],
    });
    for (const key of ["o", "O", "I"]) {
      fireEvent.keyDown(root, { key, shiftKey: key !== key.toLowerCase() });
      expect(container.querySelector("textarea"), key).toBeNull();
    }
    fireEvent.keyDown(root, { key: "A", shiftKey: true });
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("j on the last stop flashes the typing hint once", () => {
    const widen = stubBandWidth();
    const { container } = startedApp();
    act(() => widen(container));
    // The one row this conversation has is already both the first and the last stop (a snapshot
    // lands the cursor on the last row).
    fireEvent.keyDown(conversationRoot(container), { key: "j" });
    expect(container.querySelector(".band-message")!.textContent).toBe("i or Ctrl+j to type");
    vi.unstubAllGlobals();
  });

  /* The v1-ui GUI pass (2026-09-27): holding `j` to the bottom never flashed -- every step after the
     first is a repeat. The first repeat that stops after moving now flashes; the ones after it do not. */
  it("a held j that reaches the last stop by repeat flashes once", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 0);
      act(() => widen(container));
      const root = conversationRoot(container);
      fireEvent.keyDown(root, { key: "k" });
      fireEvent.keyDown(root, { key: "j" });
      fireEvent.keyDown(root, { key: "j", repeat: true });
      expect(container.querySelector(".band-message")?.textContent).toBe("i or Ctrl+j to type");
      act(() => vi.advanceTimersByTime(2100));
      expect(container.querySelector(".band-message")).toBeNull();
      fireEvent.keyDown(root, { key: "j", repeat: true });
      expect(container.querySelector(".band-message")).toBeNull();
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("does not flash on a held key's repeat -- only a fresh press", () => {
    const widen = stubBandWidth();
    const { container } = startedApp();
    act(() => widen(container));
    fireEvent.keyDown(conversationRoot(container), { key: "j", repeat: true });
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });

  it("k at the first stop stays silent -- vim's own k on line 1 (:h j)", () => {
    const widen = stubBandWidth();
    const { container } = startedApp();
    act(() => widen(container));
    fireEvent.keyDown(conversationRoot(container), { key: "k" });
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });

  it("Esc in BROWSE while a turn runs flashes that ctrl+c interrupts, and interrupts nothing", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ activeTurnId: "t1", transcript: [{ seq: 1, text: "hello" }] }), 0);
    act(() => widen(container));
    fireEvent.keyDown(conversationRoot(container), { key: "Escape" });
    expect(container.querySelector(".band-message")!.textContent).toBe("Esc does not interrupt — ctrl+c does");
    expect(posted.some((m) => m.type === "interrupt")).toBe(false);
    vi.unstubAllGlobals();
  });

  it("Esc in BROWSE idle flashes nothing (D1: Esc never interrupts, and there is nothing to say)", () => {
    const widen = stubBandWidth();
    const { container } = startedApp();
    act(() => widen(container));
    fireEvent.keyDown(conversationRoot(container), { key: "Escape" });
    expect(container.querySelector(".band-message")).toBeNull();
    vi.unstubAllGlobals();
  });

  /** R16's own live-tab counterpart (its empty-tab landing is pinned in "the session chooser"
   *  above): unchanged, the chooser's Esc always returned the conversation its own root. */
  it("Esc from the chooser over a live tab returns to the conversation root, unchanged", () => {
    const { container } = startedApp();
    const root = conversationRoot(container);
    dispatch({ kind: "chooser", open: [{ tab: 1, label: "1 new", marker: null, pending: 0, resumable: true }], records: [] });
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "Escape" });
    expect(container.querySelector(".chooser")).toBeNull();
    expect(document.activeElement).toBe(root);
  });
});

describe("v1 P7: every panel y/n accepts Y too (spec §8)", () => {
  it("Y confirms a tab close, same as y", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    fireEvent.keyDown(root, { key: "Y", shiftKey: true });
    expect(lastOfType("close_tab")).toMatchObject({ tab: 1 });
  });

  it("Y confirms close-others, same as y", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({ kind: "confirm_close_others", tabs: [2, 3], lines: ["close 2 other tabs? 1 running (y/n)"] });
    fireEvent.keyDown(root, { key: "Y", shiftKey: true });
    expect(lastOfType("close_others")).toBeDefined();
    expect(lastOfType("close_tab")).toBeUndefined();
  });

  it("n and Escape still cancel, no post", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    fireEvent.keyDown(root, { key: "n" });
    expect(lastOfType("close_tab")).toBeUndefined();
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(lastOfType("close_tab")).toBeUndefined();
  });

  it("a bare Shift is ignored -- the prompt stays open for the key that follows", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    const root = container.querySelector(".agent-ui-conversation")!;
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] });
    fireEvent.keyDown(root, { key: "Shift" });
    expect(lastOfType("close_tab")).toBeUndefined();
    // Shift alone neither answered nor cancelled the prompt: the very next key still confirms it.
    fireEvent.keyDown(root, { key: "Y", shiftKey: true });
    expect(lastOfType("close_tab")).toMatchObject({ tab: 1 });
  });
});

/* BROWSE visual mode (spec docs/superpowers/specs/2026-09-28-browse-visual-mode-design.md).
   jsdom implements `Selection` but not WebKit's own `modify()` (D1's own note, spec §3) -- this
   stub moves the live selection's focus by whole characters/words over its own text content, good
   enough to prove the WIRING end to end (a motion really moves the caret, `y` really copies
   something real off the live DOM selection). It is not a vim-exact word/line implementation and
   is not meant to be one: R1-R7 (real `Selection.modify` behaviour) are verified only in the real
   WebKit harness, `shell/tests/panel_visual_mode.rs`. */
function stubSelectionModify(): () => void {
  const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
  const original = proto.modify;
  proto.modify = function (this: Selection, alter: string, direction: string, granularity: string) {
    const node = this.focusNode;
    if (node === null) return;
    const text = node.textContent ?? "";
    let offset = this.focusOffset;
    const startAnchorNode = this.anchorNode;
    const startAnchorOffset = this.anchorOffset;
    if (granularity === "character") {
      offset = direction === "forward" ? Math.min(text.length, offset + 1) : Math.max(0, offset - 1);
      // D3/D5's own accepted quirk (spec §D3: "at a line end WebKit draws the line-break box, which
      // reads like vim's cursor past the end"): a real engine still shows a one-character block caret
      // at the very last position, which needs real layout jsdom does not have. A block caret
      // (anchor === focus, both already at `text.length`) extending forward has nowhere left to move
      // within this text node -- pull the ANCHOR back by one instead, so the stub still produces the
      // one-character selection D3 describes, without claiming jsdom modelled WebKit's own rendering.
      if (
        alter === "extend" &&
        direction === "forward" &&
        offset === this.focusOffset &&
        startAnchorNode === node &&
        startAnchorOffset === this.focusOffset &&
        offset > 0
      ) {
        this.setBaseAndExtent(node, offset - 1, node, offset);
        return;
      }
    } else if (granularity === "word") {
      if (direction === "forward") {
        let i = offset;
        while (i < text.length && /\S/.test(text[i]!)) i++;
        while (i < text.length && /\s/.test(text[i]!)) i++;
        offset = i;
      } else {
        let i = offset;
        while (i > 0 && /\s/.test(text[i - 1]!)) i--;
        while (i > 0 && /\S/.test(text[i - 1]!)) i--;
        offset = i;
      }
    } else {
      // "line" / "paragraphboundary": this stub's rows are each one text node, so both collapse to
      // that node's own start/end -- the same unit D4's own `0`/`$` and V-LINE share.
      offset = direction === "forward" ? text.length : 0;
    }
    if (alter === "move") this.collapse(node, offset);
    else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
  };
  return () => {
    if (original === undefined) delete proto.modify;
    else proto.modify = original;
  };
}

describe("BROWSE visual mode (spec 2026-09-28)", () => {
  function visualFixture() {
    return snapshotState({
      userPrompts: [{ seq: 1, text: "hello world" }],
      pendingPermissions: [{ seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
    });
  }

  /** Renders a live tab holding `visualFixture()`, lands the cursor on the prompt row (`gg`, the
   *  first row by `seq`), and returns the conversation root. */
  function startedOnPromptRow() {
    // Must run before render (`stubBandWidth`'s own doc comment): the band's `.band-message` segment
    // is dropped below `band.ts`'s pre-measurement floor (mode/pill only) until it is widened.
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(visualFixture(), 3);
    // The band's mode block draws only while this pane holds the keys (v1 polish F24) -- every
    // test below reads it, so this pane needs focus, the same as a real launch's `pane_focus` gives
    // the panel once it is clicked or `Ctrl+l`'d into.
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    gg(container);
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    expect(root.querySelector(".row-current")?.className).toContain("row-prompt");
    return { container, root };
  }

  let restoreModify: () => void;
  beforeEach(() => {
    restoreModify = stubSelectionModify();
  });
  afterEach(() => {
    restoreModify();
    window.getSelection()?.removeAllRanges();
    vi.unstubAllGlobals();
  });

  it("v enters CARET, a second v enters VISUAL, V enters V-LINE directly -- the band names which (D1, revised for 3a)", () => {
    const { container, root } = startedOnPromptRow();
    const modeBlock = () => container.querySelector('[data-testid="mode-block"]');
    fireEvent.keyDown(root, { key: "v" });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("caret");
    expect(modeBlock()?.textContent).toBe("CARET"); // band.ts's own MODE_TEXT, human-readable
    fireEvent.keyDown(root, { key: "v" });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("visual");
    expect(modeBlock()?.textContent).toBe("VISUAL");
    // D1: VISUAL's own Esc goes back to CARET, not all the way to BROWSE -- the region stays on.
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("caret");
    // CARET's own Esc ends the whole region.
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("browse");
    fireEvent.keyDown(root, { key: "V", shiftKey: true });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("vline");
    expect(modeBlock()?.textContent).toBe("V-LINE");
    // D1: V-LINE's own Esc also goes back to CARET.
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeBlock()?.getAttribute("data-mode")).toBe("caret");
  });

  /** R9 (v1 picks Task 9; idiom matrix E10): `Ctrl+[` is Esc in the region too. Until then the region
   *  read it as a modified key its table does not claim (`vswallow`: swallowed, nothing else), so a
   *  vim user reaching for it saw CARET/VISUAL stay on -- the one region change of that plan. It
   *  reaches `resolveKey` as the plain Escape `./ctrlBracket` turns it into, `repeat` and all. */
  it("R9: Ctrl+[ is Esc in the region -- VISUAL and V-LINE back to CARET, CARET to BROWSE, and a held one steps back once", () => {
    const { container, root } = startedOnPromptRow();
    const modeBlock = () => container.querySelector('[data-testid="mode-block"]');
    const mode = () => modeBlock()?.getAttribute("data-mode");
    const ctrlBracket = { key: "[", code: "BracketLeft", ctrlKey: true };

    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    expect(mode()).toBe("visual");
    expect(fireEvent.keyDown(root, ctrlBracket), "claimed, not left to the browser").toBe(false);
    expect(mode()).toBe("caret");
    // D12: the key that stepped back swallows its own repeats. The Escape this becomes carries the
    // original's `repeat`, so a held Ctrl+[ does not go on to end the whole region.
    fireEvent.keyDown(root, { ...ctrlBracket, repeat: true });
    expect(mode()).toBe("caret");
    // A fresh press is a new key and ends it, as a fresh Esc does.
    fireEvent.keyDown(root, ctrlBracket);
    expect(mode()).toBe("browse");

    fireEvent.keyDown(root, { key: "V", shiftKey: true });
    expect(mode()).toBe("vline");
    fireEvent.keyDown(root, ctrlBracket);
    expect(mode()).toBe("caret");
    fireEvent.keyDown(root, ctrlBracket);
    expect(mode()).toBe("browse");
  });

  it("refuses to start with a flash when there is no row at all", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 0); // no prompts, messages, tools or permissions: no rows
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(container.querySelector(".band-message")?.textContent).toBe("nothing to select");
  });

  it("D15: v then a, d and Shift+D answer no card in CARET, and v v in VISUAL -- the region, not BROWSE, owns those keys", () => {
    vi.useFakeTimers();
    try {
      const { root } = startedOnPromptRow();
      // The cursor must be ON the card for this test to prove anything (fix round 1, reviewer
      // finding, important, both review programs): `startedOnPromptRow` lands `gg` on the PROMPT
      // row, where BROWSE's own `a`/`d`/`D` already answer nothing (`permissionTarget` finds no
      // card there) -- so the original version of this test passed even with the region's own
      // D15 suppression deleted outright, proving nothing about it. `j` moves onto the permission
      // row.
      fireEvent.keyDown(root, { key: "j" });
      expect(root.querySelector(".row-current")?.className).toContain("row-permission");
      // `v` (CARET) before EACH key, not once before all three: D12 makes `a`/`d`/`D` themselves
      // unbound CARET keys that END the region (`landCursorOnRowKey` keeps the row cursor on the
      // card throughout), so a chained `v, a, d, Shift+D` tests `a` in CARET but `d`/`Shift+D` in
      // plain BROWSE on the same row -- which answer nothing there either, but for a different,
      // untested reason. Found while building the first version of this fix: the chained form
      // silently posted nothing for the wrong cause.
      for (const key of [{ key: "a" }, { key: "d" }, { key: "D", shiftKey: true }]) {
        fireEvent.keyDown(root, { key: "v" });
        expect(document.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
        fireEvent.keyDown(root, key);
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      }
      // The same, from VISUAL (`v v`), added for 3a.
      for (const key of [{ key: "a" }, { key: "d" }, { key: "D", shiftKey: true }]) {
        fireEvent.keyDown(root, { key: "v" });
        fireEvent.keyDown(root, { key: "v" });
        expect(document.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
        fireEvent.keyDown(root, key);
        act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      }
      expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("control for the test above: from the SAME permission row, in ordinary BROWSE (no VISUAL), a lone a DOES answer -- proving the row is a real card, not vacuous setup", () => {
    vi.useFakeTimers();
    try {
      const { root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "j" });
      expect(root.querySelector(".row-current")?.className).toContain("row-permission");
      // `a` must stand alone (S1): right after `j`, within the same guard window, it is refused as
      // typing (the same rule the test above's own comment explains) -- this control is about
      // whether the ROW answers a properly-alone `a`, not about S1 itself.
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      fireEvent.keyDown(root, { key: "a" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "allow" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("D12: Shift+Tab does nothing in CARET or VISUAL and posts no cycle_mode", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    expect(container.querySelector(".band-message")?.textContent).toContain("Shift+Tab does not act in CARET");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "Tab", shiftKey: true });
    expect(posted.filter((m) => m.type === "cycle_mode")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    expect(container.querySelector(".band-message")?.textContent).toContain("Shift+Tab does not act in VISUAL");
  });

  it("D8: y copies exactly what is highlighted, flashes, and returns to BROWSE with the row cursor on it", () => {
    const clipboard = stubClipboard();
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" }); // charwise inclusive: "he" (D3)
    fireEvent.keyDown(root, { key: "y" });
    expect(clipboard.writeText).toHaveBeenLastCalledWith("he");
    expect(container.querySelector(".band-message")?.textContent).toBe("copied 2 chars");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(container.querySelector(".row-current")?.className).toContain("row-prompt");
  });

  it("D10: a held y after it ends VISUAL does not fall through as BROWSE's own y", () => {
    const clipboard = stubClipboard();
    const { root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" });
    fireEvent.keyDown(root, { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledTimes(1);
    expect(clipboard.writeText).toHaveBeenLastCalledWith("he");
    // The SAME physical key, still down: a real auto-repeat keydown.
    fireEvent.keyDown(root, { key: "y", repeat: true });
    expect(clipboard.writeText).toHaveBeenCalledTimes(1);
    fireEvent.keyUp(root, { key: "y" });
    // Released and pressed again: an ordinary BROWSE `y` now, copying the row.
    fireEvent.keyDown(root, { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledTimes(2);
    expect(clipboard.writeText).toHaveBeenLastCalledWith("hello world");
  });

  it("Esc from VISUAL goes back to CARET, nothing copied; Esc again from CARET clears the native selection into BROWSE (D1, revised for 3a)", () => {
    const clipboard = stubClipboard();
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET, entering on "hello world"'s first character, "h"
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" }); // extend to "he" (D3: charwise inclusive)
    fireEvent.keyDown(root, { key: "Escape" });
    expect(clipboard.writeText).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    // D1/D3: CARET's own selection is one character, at the MOVING end ("cursor", which `l` just
    // advanced to "e") -- never the anchor ("h"), which a mutation landing CARET on the anchor
    // instead would still pass a bare length-1 check with (it would read "h").
    expect(window.getSelection()?.toString()).toBe("e");
    fireEvent.keyDown(root, { key: "Escape" });
    expect(clipboard.writeText).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
  });

  it("D12: any other unbound key ends CARET and says so, never its BROWSE meaning", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "d" });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      expect(container.querySelector(".band-message")?.textContent).toBe("CARET ended: d is not a CARET key (v selects)");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("D12: any other unbound key ends VISUAL (the whole region) and says so, never its BROWSE meaning", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "d" });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      expect(container.querySelector(".band-message")?.textContent).toBe("VISUAL ended: d is not a VISUAL key (y copies)");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("D12: pane_focus false ends the region (CARET or VISUAL) and clears the selection", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    dispatch({ kind: "pane_focus", focused: false });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    dispatch({ kind: "pane_focus", focused: false });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
  });

  it("D12: a tab switch ends the region (CARET or VISUAL)", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    dispatch({ kind: "tabs", active: 2, tabs: [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
  });

  it("§7 finding 1: a click into the card's reason box ends the region, and its own Enter then denies", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "j" }); // onto the permission card row
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    const reasonBox = container.querySelector<HTMLInputElement>(".permission-card input")!;
    fireEvent.pointerDown(reasonBox);
    fireEvent.focus(reasonBox);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    fireEvent.keyDown(reasonBox, { key: "Enter" });
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "deny" });
  });

  it("§7 finding 2: a bare Shift keydown then $ keeps CARET, then keeps VISUAL, then V converts it to V-LINE", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "Shift" });
    fireEvent.keyDown(root, { key: "$", shiftKey: true });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "Shift" });
    fireEvent.keyDown(root, { key: "$", shiftKey: true });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    fireEvent.keyDown(root, { key: "Shift" });
    fireEvent.keyDown(root, { key: "V", shiftKey: true });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("vline");
  });

  it("§7 finding 5: a leader on v means v starts the leader, and V then v reaches VISUAL", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      // `binding` (testFixtures.ts): builds a `PanelTable` entry the `keymap` envelope carries.
      dispatch({
        kind: "keymap",
        prefix: "Ctrl+b",
        window: [],
        prefixKeys: [],
        newTabChord: "Ctrl+b c",
        panel: { ...EMPTY_PANEL_TABLE, leader: "v", leaderLabel: "v", leaderSource: "mapleader", bindings: [binding(["<leader>", "d"], "tab.close")] },
      });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      fireEvent.keyDown(root, { key: "v" });
      // The leader claimed it: no VISUAL, no flash naming it as unbound.
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      // Fix round 3 (review finding 5): the first version asserted only that negative half, which a
      // `v` swallowed by anything at all would satisfy. The positive half: `v` really STARTED the
      // leader -- its which-key box draws, and the sequence it began completes (`<leader>d` runs
      // `tab.close`).
      act(() => vi.advanceTimersByTime(WHICH_KEY_DELAY_MS));
      expect(container.querySelector(".which-key-box"), "v, the leader, draws the leader's which-key box").not.toBeNull();
      posted.length = 0;
      fireEvent.keyDown(root, { key: "d" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(posted.filter((m) => m.type === "tab_verb"), "<leader>d, begun by v, ran tab.close").toEqual([
        expect.objectContaining({ type: "tab_verb", verb: "close" }),
      ]);
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      fireEvent.keyDown(root, { key: "V", shiftKey: true });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("vline");
      fireEvent.keyDown(root, { key: "v" });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    } finally {
      vi.useRealTimers();
    }
  });

  it("a plain (non-leader) panel binding on V wins over VISUAL entry too -- D2's rule is not leader-only", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      dispatch({
        kind: "keymap",
        prefix: "Ctrl+b",
        window: [],
        prefixKeys: [],
        newTabChord: "Ctrl+b c",
        panel: { ...EMPTY_PANEL_TABLE, bindings: [binding(["V"], "tab.close")] },
      });
      // Past the typing guard's window: right after `startedOnPromptRow`'s own `gg`, a single-key
      // table binding is refused as typing (R2-2, "k then L at 80 ms") -- which is why the first
      // version of this test, pressing `V` at once, saw "browse" without the binding ever running.
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      posted.length = 0;
      fireEvent.keyDown(root, { key: "V", shiftKey: true });
      // A single-key table binding runs `TYPING_GUARD_MS` later, standing alone, the same as `L`'s
      // own single-key default does (R2-2) -- the contrapositive of what this assertion proves: had
      // the table lookup NOT beaten `resolveKey`'s own `V` -> VISUAL row, this would read "vline"
      // instead of "browse", deferred or not.
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      // Fix round 3 (review finding 5): and the positive half spec §3 names -- "V runs it". Without
      // this, a `V` that did nothing at all passed as well as one that ran the binding.
      expect(posted.filter((m) => m.type === "tab_verb"), "the init.lua row on V ran: tab.close").toEqual([
        expect.objectContaining({ type: "tab_verb", verb: "close" }),
      ]);
    } finally {
      vi.useRealTimers();
    }
  });

  // Fix round 1 (finding 9 of both review programs, "vitest cases required by spec §3 are
  // missing"): D12's routes each call `exitVisual` explicitly now -- the routes just above this
  // describe's own tests never had a test proving it. Mutation testing found removing any one of
  // these calls still passed the suite.
  it.each([
    ["hint_collect", { kind: "hint_collect", sessionId: 1 }],
    ["begin_rename", { kind: "begin_rename", tab: 1, current: null }],
    ["chooser", { kind: "chooser", open: [], records: [] }],
    ["confirm_close", { kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] }],
    ["confirm_close_others", { kind: "confirm_close_others", tabs: [2], lines: ["close 1 other tab? (y/n)"] }],
  ])("D12: %s ends CARET and clears the selection it built", (_name, envelope) => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    expect(window.getSelection()?.toString()).not.toBe("");
    dispatch(envelope);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
  });

  it.each([
    ["hint_collect", { kind: "hint_collect", sessionId: 1 }],
    ["begin_rename", { kind: "begin_rename", tab: 1, current: null }],
    ["chooser", { kind: "chooser", open: [], records: [] }],
    ["confirm_close", { kind: "confirm_close", tab: 1, lines: ["close 1? (y/n)"] }],
    ["confirm_close_others", { kind: "confirm_close_others", tabs: [2], lines: ["close 1 other tab? (y/n)"] }],
  ])("D12: %s ends VISUAL and clears the selection it built (3a: the D14 exit runs from CARET above and VISUAL here)", (_name, envelope) => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "l" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    expect(window.getSelection()?.toString()).not.toBe("");
    dispatch(envelope);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
  });

  it("D12: confirm_bypass (Shift+Tab's own y/n prompt) ends the region too, from CARET and from VISUAL", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["Switch to bypass? (y/n)"] });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    // Close the still-open y/n prompt itself before re-entering: any key but a counted `y` cancels
    // it (spec §3.4), and a `v` aimed at a live prompt must answer THAT, not start the region.
    fireEvent.keyDown(root, { key: "Escape" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 8, lines: ["Switch to bypass? (y/n)"] });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
  });

  it("D15/§7 finding 1, widened: a keydown dispatched directly on the reason box, or on Approve, never reaches CARET's own table -- the capture handler's foreign-target swallow", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "j" }); // onto the permission card row
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    // A real click already ends the region before its own keydown (D12/D15) -- this asserts the
    // OTHER half of D15's own rule directly: "a key whose target is not the root ends the region
    // and is swallowed, so no descendant sees it." Dispatched straight onto the controls, bypassing
    // the pointer-down exit, so a stray focus change (not a click) is what is under test.
    const reasonBox = container.querySelector<HTMLInputElement>(".permission-card input")!;
    const approve = container.querySelector<HTMLButtonElement>('button[data-nav-action="allow"]')!;
    fireEvent.keyDown(reasonBox, { key: "a" });
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    // Back into CARET, then straight at the button.
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(approve, { key: "v" }); // CARET's own select key, targeted at the button instead of root
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
  });

  it("D15/§7 finding 1, the same from VISUAL: a keydown dispatched directly on the reason box, or on Approve, never reaches VISUAL's own table", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "j" }); // onto the permission card row
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    const reasonBox = container.querySelector<HTMLInputElement>(".permission-card input")!;
    const approve = container.querySelector<HTMLButtonElement>('button[data-nav-action="allow"]')!;
    fireEvent.keyDown(reasonBox, { key: "a" });
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    // Back into VISUAL, then straight at the button.
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(approve, { key: "y" }); // VISUAL's own copy key, targeted at the button instead of root
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
  });

  it("D11: a delta arriving during VISUAL leaves the reply's DOM untouched until Esc -- the freeze itself, not just that it is documented", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 3,
      throughRevision: 5,
      events: [
        { type: "turn_started", turn_id: "t1" },
        { type: "content_delta", turn_id: "t1", kind: "text", text: "a new reply" },
      ],
    });
    // The event reached the projection (the band/tray would count it) but the FROZEN list must not
    // draw it -- removing the freeze (`state={state}` instead of `frozenSnapshot?.state ?? state`)
    // still passes every other test in this describe, which is why this one exists.
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    fireEvent.keyDown(root, { key: "Escape" });
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(1);
    expect(container.querySelector(".row-assistant")!.textContent).toContain("a new reply");
  });

  it("blocking finding (both review programs): a session ending during VISUAL, then a tab switch to a DIFFERENT card, does not let a stale frozen card's key answer the new tab's card", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "j" }); // onto tab 1's own "rm build" card
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "v" });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
      // Tab 1's session ends while VISUAL is on -- the `sessionEnded` effect writes `mode` directly,
      // never through `exitVisual`, so before the fix this left `frozenSnapshot` (and the DOM
      // selection) exactly as they were.
      dispatch({
        kind: "events",
        tab: 1,
        fromRevision: 3,
        throughRevision: 4,
        events: [{ type: "session_closed", reason: "provider exited" }],
      });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      // A second tab, with its own DIFFERENT, dangerous card.
      dispatch({
        kind: "tabs",
        active: 2,
        tabs: [
          { id: 1, number: 1, label: "1", name: null, state: "closed", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null },
          { ...LIVE_TAB, id: 2, number: 2, label: "2" },
        ],
      });
      dispatch({
        kind: "snapshot",
        tab: 2,
        throughRevision: 1,
        state: snapshotState({ pendingPermissions: [{ seq: 1, permissionId: "pDANGEROUS", toolUseId: null, toolName: "Bash", input: { command: "rm -rf build" } }] }),
      });
      dispatch({ kind: "focus_permission", tab: 2 });
      // The panel must be showing tab 2's own card now, never tab 1's stale "rm build" one. (Both
      // cards say "Permission requested: Bash" -- the tool name, not the command -- so that heading
      // alone cannot tell them apart; the command text is what must differ, and "rm build" is not a
      // substring of "rm -rf build".)
      expect(container.textContent).toContain("rm -rf build");
      expect(container.textContent).not.toContain("rm build");
      const liveRoot = container.querySelector(".agent-ui-conversation")! as HTMLElement;
      fireEvent.keyDown(liveRoot, { key: "a" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "pDANGEROUS", decision: "allow" });
    } finally {
      vi.useRealTimers();
    }
  });

  const modeOf = (container: HTMLElement) => container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");

  /* Fix round 2 (reviewer finding, important): the `handoff` and `error` arms reset the whole state
     to `initialState()` -- status `starting`, so not an ended session, and `mode` untouched -- and
     neither ever called `exitVisual`. VISUAL, and the conversation it froze, outlived the
     conversation. Each case below has a control run without `v`, so the setup is proven real. */
  /** `between` runs after the `handoff` envelope and before the `tabs` one that follows it, so a
   *  test can look at the arm's own effect before the `[sessionStarted]` backstop gets a turn. */
  function handOffTab1(between: () => void = () => {}) {
    dispatch({
      kind: "handoff",
      tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    between();
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
  }

  it("control for the handoff case below: without VISUAL, Shift+Tab on the empty tab a handoff leaves posts cycle_mode", () => {
    const { container } = startedOnPromptRow();
    handOffTab1();
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "Tab", shiftKey: true });
    expect(lastOfType("cycle_mode")).toBeDefined();
  });

  it("fix round 2: a handoff during CARET ends it -- the empty tab's Shift+Tab cycles the mode instead of saying Esc first", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    // The arm itself ends the region (the tab is still `live` here, so the `[sessionStarted]` backstop
    // has not run yet): no frozen copy of the handed-over conversation is left on screen.
    handOffTab1(() => {
      expect(modeOf(container)).toBe("browse");
      expect(container.textContent).not.toContain("hello world");
    });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "Tab", shiftKey: true });
    expect(lastOfType("cycle_mode")).toBeDefined();
    expect(container.textContent).not.toContain("Esc first");
  });

  /** The tab fails while VISUAL is on, then Rust restarts it: `tabs` live again and the NEW
   *  conversation's snapshot. (Rust's restart sends no `arrive`/`enter_input`, so nothing but this
   *  panel's own reset can end VISUAL here.) */
  function failAndRestartTab1(between: () => void = () => {}) {
    dispatch({ kind: "error", tab: 1, message: "the session died" });
    between();
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: "the session died" }] });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: snapshotState({ userPrompts: [{ seq: 1, text: "a brand new conversation" }] }) });
  }

  it("control for the error case below: without VISUAL, a failed-then-restarted tab shows its new conversation", () => {
    const { container } = startedOnPromptRow();
    failAndRestartTab1();
    expect(container.textContent).toContain("a brand new conversation");
    expect(container.textContent).not.toContain("rm build");
    expect(modeOf(container)).toBe("browse");
  });

  it("fix round 2: an error during CARET ends it -- the restarted tab shows its own conversation, never the frozen one with the dead session's card", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    // The `error` arm itself ends the region, before the `tabs` envelope that would let the
    // `[sessionStarted]` backstop do it.
    failAndRestartTab1(() => expect(modeOf(container)).toBe("browse"));
    expect(container.textContent).toContain("a brand new conversation");
    expect(container.textContent).not.toContain("hello world");
    expect(container.textContent).not.toContain("rm build");
    expect(modeOf(container)).toBe("browse");
  });

  it("fix round 2's backstop: a tab that leaves `live` by a bare `tabs` envelope (no error, no handoff) ends CARET too", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    // Nothing but `tabs` says the conversation is gone: only the `[sessionStarted]` effect sees it.
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 9, state: snapshotState({ userPrompts: [{ seq: 1, text: "a brand new conversation" }] }) });
    expect(container.textContent).toContain("a brand new conversation");
    expect(container.textContent).not.toContain("rm build");
    expect(modeOf(container)).toBe("browse");
  });

  /* Spec §3, the review's finding-3 case: App's `timeline` stays live while `MessageList` draws the
     frozen one, so the two are joined by key. A card linked to an EARLIER tool call arrives while
     VISUAL is on: live `[tool, reply]` becomes `[tool, card, reply]` (`timeline.ts`'s anchoring), the
     frozen list still has two rows. Handing `MessageList` the live index (2) would name no frozen row;
     the key names the reply in both. */
  function linkedFixture() {
    return snapshotState({
      activeTurnId: "t1",
      toolCalls: [{ seq: 1, toolUseId: "tu1", name: "Bash", input: { command: "ls" }, result: null, turnId: "t1" }],
      transcript: [{ seq: 2, text: "the reply under the cursor" }],
    });
  }
  function startedOnLinkedReply() {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(linkedFixture(), 3);
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    // A snapshot lands the cursor on the last row: the reply.
    expect(root.querySelector(".row-current")?.textContent).toContain("the reply under the cursor");
    return { container, root };
  }
  function linkedCardArrives() {
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 3,
      throughRevision: 4,
      events: [{ type: "permission_requested", permission_id: "p-linked", tool_use_id: "tu1", tool_name: "Bash", input: { command: "ls" } }],
    });
  }

  it("§7 finding 3: a linked card arriving before the selected reply keeps MessageList's current row on the reply, and Esc lands the live cursor there", () => {
    const { container, root } = startedOnLinkedReply();
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    linkedCardArrives();
    // Frozen: the card is not drawn yet, and the current row is still the reply, found by key.
    expect(container.querySelector(".row-permission")).toBeNull();
    expect(root.querySelector(".row-current")?.textContent).toContain("the reply under the cursor");
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf(container)).toBe("browse");
    // Live again: the card is drawn between the tool and the reply, and the cursor is on the reply,
    // not on whatever row now sits at the reply's old index (the card).
    expect(container.querySelector(".row-permission")).not.toBeNull();
    const rows = Array.from(root.querySelectorAll(".message-list [data-nav-stop='row']"));
    expect(rows.map((r) => r.classList.contains("row-permission"))).toEqual([false, true, false]);
    expect(root.querySelector(".row-current")?.textContent).toContain("the reply under the cursor");
  });

  it("D12: a card arriving does not end CARET -- an exit nobody asked for would turn the next d or y into BROWSE's", () => {
    const { container, root } = startedOnLinkedReply();
    fireEvent.keyDown(root, { key: "v" });
    linkedCardArrives();
    expect(modeOf(container)).toBe("caret");
    expect(window.getSelection()?.toString()).not.toBe("");
  });

  it("D8: a selection changed under VISUAL (a mouse drag, say) copies nothing, says so, and ends VISUAL", () => {
    const clipboard = stubClipboard();
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" });
    // Replace the live selection without any DOM change (no observer fires) and without a pointer
    // event (which would end VISUAL first): what a drag the page never saw leaves behind.
    const promptText = Array.from(root.querySelectorAll(".row-prompt *"))
      .flatMap((el) => Array.from(el.childNodes))
      .find((n) => n.nodeType === Node.TEXT_NODE && (n.textContent ?? "").includes("hello world"))!;
    window.getSelection()!.setBaseAndExtent(promptText, 0, promptText, 5);
    expect(window.getSelection()!.toString()).toBe("hello");
    fireEvent.keyDown(root, { key: "y" });
    expect(clipboard.writeText).not.toHaveBeenCalled();
    expect(container.querySelector(".band-message")?.textContent).toBe("selection changed under it — nothing copied; v to start again");
    expect(modeOf(container)).toBe("browse");
  });

  // Spec §3/§7 finding 1 tests the two D12 exits together (a click into the reason box is both a
  // pointer press and a focus); each alone must end VISUAL too, or removing either passes the suite.
  it("D12: a pointer press alone (on text that takes no focus) ends CARET", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    const text = root.querySelector(".row-prompt")!.lastElementChild!;
    fireEvent.pointerDown(text);
    expect(modeOf(container)).toBe("browse");
  });

  it("D12: focus alone landing on a control (no pointer press) ends CARET", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "j" }); // onto the permission card row
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    const reasonBox = container.querySelector<HTMLInputElement>(".permission-card input")!;
    act(() => reasonBox.focus());
    expect(modeOf(container)).toBe("browse");
  });

  it("D15: a CARET motion key aimed at the reason box (not the root) ends CARET instead of moving the caret", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "j" }); // onto the permission card row
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    const reasonBox = container.querySelector<HTMLInputElement>(".permission-card input")!;
    // `l` is a motion: taken as a CARET key it would move the caret and leave CARET on. A key on a
    // foreign target must end the region and be swallowed instead (D15).
    const notPrevented = fireEvent.keyDown(reasonBox, { key: "l" });
    expect(notPrevented).toBe(false);
    expect(modeOf(container)).toBe("browse");
    expect(window.getSelection()?.toString()).toBe("");
  });

  /* Fix round 3 (review finding 2, important): D13's "VISUAL keys count as typing for what follows"
     had no test -- the capture handler's `typingGuard.onKey` replaced by a no-op passed the suite.
     Without it the guard never hears a key VISUAL takes (the capture handler stops their
     propagation, so `onKeyDown`'s own call never runs for them), and a BROWSE `d` typed right after
     the key that ENDED VISUAL looks alone: "ver" then `d` -- prose after a stray `v`, D2's own
     "verify" example -- denied the card. The reviewer's probe, kept as the test. */
  it("D13: the keys VISUAL takes count as typing -- a d right after the key that ended VISUAL answers no card", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "j" });
      expect(root.querySelector(".row-current")?.className).toContain("row-permission");
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      fireEvent.keyDown(root, { key: "v" });
      act(() => vi.advanceTimersByTime(120));
      fireEvent.keyDown(root, { key: "e" });
      act(() => vi.advanceTimersByTime(120));
      fireEvent.keyDown(root, { key: "r" }); // not a VISUAL key: ends VISUAL (vend)
      expect(modeOf(container)).toBe("browse");
      act(() => vi.advanceTimersByTime(120));
      fireEvent.keyDown(root, { key: "d" }); // BROWSE's d, 120ms after r
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
      // Control: the same row, a d standing alone this time, does deny -- the card is real.
      fireEvent.keyDown(root, { key: "d" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 10));
      expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "deny" });
    } finally {
      vi.useRealTimers();
    }
  });

  /* Fix round 3 (review finding 4, minor): D10's modified keys, end to end -- `keymap.test.ts` pins
     `vswallow`; this pins what swallowing means on the page: the key's default is prevented, VISUAL
     stays, and the selection does not move (Alt+j is not j). */
  it("D10: Ctrl+o and Alt+j in VISUAL are swallowed -- prevented, VISUAL stays, the selection unchanged", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" });
    const before = window.getSelection()!.toString();
    expect(before).toBe("he");
    expect(fireEvent.keyDown(root, { key: "o", ctrlKey: true }), "Ctrl+o's default is prevented").toBe(false);
    expect(modeOf(container)).toBe("visual");
    expect(fireEvent.keyDown(root, { key: "j", altKey: true }), "Alt+j's default is prevented").toBe(false);
    expect(modeOf(container)).toBe("visual");
    expect(window.getSelection()!.toString()).toBe(before);
  });

  /* Fix round 3 (review finding 3, important): D11 says nothing indexes the DOM by the live cursor
     while VISUAL is on. Three things did, all reached by the reviewer's probe: a queued prompt sent at
     a turn's end (`landOnPromptRef`) moves the LIVE cursor to a row the frozen list does not have --
     here live index 3, while the frozen list's row 3 is an unrelated tool row -- and then the
     `[cursor]` effect scrolled that frozen row into view, `MessageList`'s `?? cursor` fallback drew it
     as the current row, and R1's scroll clamp read the frozen rows by the live index and wrote a
     frozen index back as the live cursor. */
  function queuedPromptFixture() {
    const tool = (seq: number) => ({ seq, toolUseId: `tu${seq}`, name: "Read", input: { file_path: `/x/f${seq}.rs` }, result: { content: `file ${seq}`, isError: false }, turnId: "t1" });
    return snapshotState({
      activeTurnId: "t1",
      userPrompts: [{ seq: 1, text: "first prompt" }],
      transcript: [{ seq: 2, text: "the reply being read" }],
      toolCalls: [tool(3), tool(4), tool(5)],
    });
  }
  function queuedPromptSent() {
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 9,
      throughRevision: 11,
      events: [
        { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "", stop_reason: "end_turn", usage: null },
        { type: "user_prompt_submitted", text: "a queued prompt" },
      ],
    });
  }
  function rect(top: number, bottom: number) {
    return { top, bottom, left: 0, right: 100, width: 100, height: bottom - top, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
  }

  it("D11: a queued prompt sent at a turn's end during VISUAL reveals no frozen row, draws no stray current row, and the scroll clamp writes nothing back", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(queuedPromptFixture(), 9);
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    gg(container);
    fireEvent.keyDown(root, { key: "j" });
    expect(root.querySelector(".row-current")?.textContent).toContain("the reply being read");
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    const frozenRows = () => Array.from(root.querySelectorAll(".message-list [data-nav-stop='row']")).map((r) => r.textContent);
    const before = frozenRows();
    expect(before).toHaveLength(5); // prompt, reply, three Reads (a running turn folds no run)
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();
    queuedPromptSent();
    expect(modeOf(container)).toBe("caret");
    expect(frozenRows(), "the list on screen is still the frozen one").toEqual(before);
    expect(scrollIntoView, "no frozen row is revealed for the live cursor").not.toHaveBeenCalled();
    expect(root.querySelector(".row-current"), "the live cursor's row is not in the frozen list: no current row at all").toBeNull();

    // R1's clamp, with geometry that would move the cursor: the frozen row at the live index (3) is
    // off screen above, the first row visible. A clamp reading the frozen rows would write 0 back.
    const list = root.querySelector<HTMLElement>(".message-list")!;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    list.getBoundingClientRect = () => rect(0, 100);
    rows.forEach((row, i) => (row.getBoundingClientRect = () => (i === 0 ? rect(0, 50) : rect(-300, -200))));
    fireEvent.scroll(list);
    expect(root.querySelector(".row-current"), "the clamp wrote no frozen index back as the live cursor").toBeNull();

    // Esc: the live list, with the queued prompt, and the cursor back on the row the region was on (D9).
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf(container)).toBe("browse");
    expect(root.textContent).toContain("a queued prompt");
    expect(root.querySelector(".row-current")?.textContent).toContain("the reply being read");
  });

  /* Found while fixing the above: `focus_permission`'s own reveal read the DOM by a live index right
     after `exitVisual` -- which has only ASKED for the thawed list, so the DOM there was still the
     frozen one. A card that arrived during VISUAL is not in it: the row was never revealed. */
  it("D11: focus_permission right after VISUAL reveals the card in the live list, not a frozen row at its index", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(
      snapshotState({
        activeTurnId: "t1",
        toolCalls: [{ seq: 1, toolUseId: "tu1", name: "Bash", input: { command: "ls" }, result: null, turnId: "t1" }],
        transcript: [{ seq: 2, text: "the reply" }],
        pendingPermissions: [{ seq: 3, permissionId: "p2", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }],
      }),
      4, // the next event gets seq 4: p2 (seq 3) stays the oldest card
    );
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    // A snapshot lands on the last row: the card.
    expect(root.querySelector(".row-current")?.textContent).toContain("rm build");
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf(container)).toBe("caret");
    // A card linked to the EARLIER tool call arrives: live [tool, card, reply, p2], frozen still
    // [tool, reply, p2]. The live cursor follows p2 by key to index 3; the frozen list has no row 3.
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 4,
      throughRevision: 5,
      events: [{ type: "permission_requested", permission_id: "p-linked", tool_use_id: "tu1", tool_name: "Bash", input: { command: "ls" } }],
    });
    expect(root.querySelectorAll(".row-permission"), "frozen: the linked card is not drawn yet").toHaveLength(1);
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();
    // The oldest waiting card is p2 (lowest seq), which the cursor is already on.
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(modeOf(container)).toBe("browse");
    const revealed = scrollIntoView.mock.instances.map((el: Element) => el.textContent ?? "");
    expect(revealed.some((text) => text.includes("rm build")), `revealed: ${JSON.stringify(revealed)}`).toBe(true);
    expect(root.querySelector(".row-current")?.textContent).toContain("rm build");
  });

  /* Fix round 4: `hint_collect`'s dispatch arm had the exact gap `focus_permission`'s reveal had
     before the D11 test above fixed it -- `exitVisual` only ASKS `MessageList` to draw the live list;
     it does not repaint synchronously, so reading `containerRef.current` right there, before that
     repaint, could still be the frozen VISUAL one. A row that arrived during VISUAL (a linked
     permission card, with its own Approve/Deny/reason controls) would then get no HINT label at all.
     A probe with `getBoundingClientRect` stubbed nonzero (jsdom lays out nothing, so without this
     stub every target -- old or new -- measures zero-size and is filtered out regardless of the bug)
     found 3 targets collected while VISUAL still framed the collect, 7 once thawed. */
  it("D11: hint_collect right after VISUAL counts the thawed list, not the frozen one it interrupts", () => {
    const originalRect = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
      return { top: 0, bottom: 10, left: 0, right: 100, width: 100, height: 10, x: 0, y: 0, toJSON: () => ({}) } as DOMRect;
    };
    try {
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(
        snapshotState({
          toolCalls: [{ seq: 1, toolUseId: "tu1", name: "Bash", input: { command: "ls" }, result: null, turnId: "t1" }],
        }),
        2,
      );
      dispatch({ kind: "pane_focus", focused: true });
      act(() => widen(container));
      const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
      // A snapshot lands on the last (and only) row: the live Bash tool call.
      expect(root.querySelector(".row-current")?.className).toContain("row-tool");
      fireEvent.keyDown(root, { key: "v" });
      expect(modeOf(container)).toBe("caret");
      // A card linked to that same call arrives while CARET is on -- not in the frozen list `v`
      // captured, same fixture shape as the D11 `focus_permission` test just above.
      dispatch({
        kind: "events",
        tab: 1,
        fromRevision: 2,
        throughRevision: 3,
        events: [{ type: "permission_requested", permission_id: "p-linked", tool_use_id: "tu1", tool_name: "Bash", input: { command: "ls" } }],
      });
      dispatch({ kind: "hint_collect", sessionId: 1 });
      const withVisual = lastOfType("hint_targets");
      expect(withVisual).toMatchObject({ session_id: 1 });
      // `hint_collect`'s own `exitVisual` has thawed the list by now (`mode` reads "browse"): a second
      // HINT, right after, collects the identical live DOM with no VISUAL in the way at all -- the
      // control this test is actually about.
      expect(modeOf(container)).toBe("browse");
      dispatch({ kind: "hint_collect", sessionId: 2 });
      const withoutVisual = lastOfType("hint_targets");
      expect(withoutVisual).toMatchObject({ session_id: 2 });
      expect(
        withVisual!.count,
        `${withVisual!.count} targets collected with VISUAL still framing the collect, ${withoutVisual!.count} once thawed`,
      ).toBe(withoutVisual!.count);
    } finally {
      Element.prototype.getBoundingClientRect = originalRect;
    }
  });

  /* Fix round 3 (review finding 1, important): the real-WebKit W6 (`shell/tests/panel_visual_mode.rs`)
     could not pass. A finished result is folded by default (`MessageList` renders
     `renderToolCall(call, expanded[key] === true)`, `expanded` starts `{}`), and `/` matches folded
     text without unfolding it -- so `V` started on the invocation line and the first `j` left the row
     with no `.tool-result-body` to be in. D6's own route is `Enter` before `v`. And from the
     invocation line, 40 `j` end on "build line 040", not 041. Proved here with a `modify` stub that
     moves by hard line (each `pre` line is one screen line in W6's fixture, and the invocation is one
     line): the same keys W6 sends, in the replay's own row order. */
  function stubLineModify(): () => void {
    const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
    const original = proto.modify;
    const blockOf = (node: Node) => (node.parentElement?.closest("pre, p, li, td, th, div") ?? null) as Element | null;
    const textNodes = () => {
      const out: Text[] = [];
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) if ((n.textContent ?? "").length > 0) out.push(n as Text);
      return out;
    };
    proto.modify = function (this: Selection, alter: string, direction: string, granularity: string) {
      const node = this.focusNode;
      if (node === null) return;
      const text = node.textContent ?? "";
      let target: Node = node;
      let offset = this.focusOffset;
      if (granularity === "line" && direction === "forward") {
        const nl = text.indexOf("\n", offset);
        if (nl !== -1 && nl + 1 < text.length) offset = nl + 1;
        else {
          // The next line starts in the next text node that is in a different block.
          const all = textNodes();
          const block = blockOf(node);
          const next = all.slice(all.indexOf(node as Text) + 1).find((n) => blockOf(n) !== block);
          if (next === undefined) return;
          target = next;
          offset = 0;
        }
      } else if (granularity === "paragraphboundary") {
        if (direction === "forward") {
          const nl = text.indexOf("\n", offset);
          offset = nl === -1 ? text.length : nl;
        } else {
          const nl = text.lastIndexOf("\n", Math.max(0, offset - 1));
          offset = nl === -1 ? 0 : nl + 1;
        }
      } else if (granularity === "character") {
        offset = direction === "forward" ? Math.min(text.length, offset + 1) : Math.max(0, offset - 1);
      } else return;
      if (alter === "move") this.collapse(target, offset);
      else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, target, offset);
    };
    return () => {
      if (original === undefined) delete proto.modify;
      else proto.modify = original;
    };
  }

  it("W6's route, in jsdom: / finds the folded result, Enter unfolds it, V then 40 j stays in the box and ends on build line 040", () => {
    restoreModify();
    restoreModify = stubLineModify();
    const clipboard = stubClipboard();
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const out = Array.from({ length: 80 }, (_, i) => `build line ${String(i + 1).padStart(3, "0")}: ok`).join("\n");
    // `replay_main`'s own order since fix round 3: the waiting card BEFORE the tool call, so a row
    // follows the card (W7 crosses its controls).
    dispatchLiveTab(
      snapshotState({
        userPrompts: [{ seq: 1, text: "The quick brown fox jumps over the lazy dog" }],
        transcript: [{ seq: 2, text: "a reply" }],
        pendingPermissions: [{ seq: 3, permissionId: "perm_edit", toolUseId: "toolu_edit", toolName: "Edit", input: { file_path: "/x/lib.rs", old_string: "old text here", new_string: "new text here" } }],
        toolCalls: [{ seq: 4, toolUseId: "toolu_bash", name: "Bash", input: { command: "cat build.log" }, result: { content: out, isError: false }, turnId: "t1" }],
      }),
      9,
    );
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(container));
    const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
    fireEvent.keyDown(root, { key: "/" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    fireEvent.change(input, { target: { value: "build line 001" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(root.querySelector(".row-current")?.className).toContain("row-tool");
    expect(container.querySelector(".tool-result-body"), "/ found the row, but its result is still folded").toBeNull();
    fireEvent.keyDown(root, { key: "Enter" });
    expect(container.querySelector(".tool-result-body"), "Enter draws the result (D6)").not.toBeNull();
    fireEvent.keyDown(root, { key: "V", shiftKey: true });
    expect(modeOf(container)).toBe("vline");
    const sel = window.getSelection()!;
    expect(sel.anchorNode?.parentElement?.closest(".tool-card-bash"), "V starts on the invocation line").not.toBeNull();
    for (let step = 1; step <= 40; step++) {
      fireEvent.keyDown(root, { key: "j" });
      expect(sel.focusNode?.parentElement?.closest(".tool-result-body"), `j #${step} left the box`).not.toBeNull();
    }
    fireEvent.keyDown(root, { key: "y" });
    const calls = clipboard.writeText.mock.calls;
    const text = calls[calls.length - 1]?.[0] as string;
    expect(text).toContain("$ cat build.log");
    expect(text).toContain("build line 001");
    expect(text).toContain("build line 040");
    expect(text, "V-LINE ends at the cursor's own line").not.toContain("build line 041");
  });

  // Revision for 3a (§9), D5: gg/G place the caret (or extend the selection to) the list's first/
  // last selectable character -- never through Selection.modify (the fake stubbed by beforeEach
  // would show it in `sel.calls` if it were), so this is proved from CARET's OWN one-character
  // selection landing on the very first/last character of the fixture's two rows.
  it("gg/G place the caret at the conversation's first/last selectable character (D5, added for 3a)", () => {
    const { container, root } = startedOnPromptRow();
    expect(container.querySelector(".row-current")?.className).toContain("row-prompt");
    fireEvent.keyDown(root, { key: "v" }); // CARET, starting on the prompt row
    fireEvent.keyDown(root, { key: "G", shiftKey: true });
    expect(window.getSelection()!.toString().length).toBe(1);
    // The row cursor does not move until the region ends -- D1's Esc lands it where the caret
    // walked to.
    fireEvent.keyDown(root, { key: "Escape" });
    expect(container.querySelector(".row-current")?.className).toContain("row-permission");
    fireEvent.keyDown(root, { key: "v" }); // CARET again, now starting on the permission row
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(container.querySelector(".row-current")?.className).toContain("row-prompt");
  });

  it("> quotes the selection into the draft and enters INPUT with the caret at the end; a second quote appends below (D10, added for 3a)", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "v" }); // CARET
      fireEvent.keyDown(root, { key: "v" }); // VISUAL
      fireEvent.keyDown(root, { key: "w" }); // "hello w" -- vim's own inclusive w (D4/D9)
      posted.length = 0;
      fireEvent.keyDown(root, { key: ">", shiftKey: true });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("input");
      const textarea = container.querySelector<HTMLTextAreaElement>("textarea")!;
      expect(document.activeElement).toBe(textarea);
      expect(textarea.value).toContain("> hello w\n\n");
      expect(textarea.selectionStart).toBe(textarea.value.length);
      // Exactly one draft posted, the full text, nothing sent and no turn started.
      act(() => vi.advanceTimersByTime(300));
      const draftPosts = posted.filter((m) => m.type === "draft");
      expect(draftPosts).toHaveLength(1);
      expect(draftPosts[0]).toMatchObject({ text: textarea.value });
      expect(posted.some((m) => m.type === "send" || m.type === "send_now")).toBe(false);
      // A second quote, from a fresh selection, appends below the first. Escape leaves INPUT from
      // the textarea itself (`Composer`'s own handler, the same convention `typeAndLeaveCaretAt3`
      // above uses) -- dispatching it on `root` instead does not reach the composer's box.
      fireEvent.keyDown(textarea, { key: "Escape" }); // back to BROWSE
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "v" });
      fireEvent.keyDown(root, { key: "l" });
      fireEvent.keyDown(root, { key: ">", shiftKey: true });
      // Composer only renders a `<textarea>` while `mode === "input"` (its own return JSX): leaving
      // to BROWSE above unmounted the first one, and this `>` mounts a fresh one -- `textarea` is a
      // stale, detached reference by now, so the second half re-queries rather than reusing it.
      const secondTextarea = container.querySelector<HTMLTextAreaElement>("textarea")!;
      expect(secondTextarea.value).toBe(`> hello w\n\n> he\n\n`);
    } finally {
      vi.useRealTimers();
    }
  });

  // D10, added in fix round 2 (reviewer finding, minor): `>` must move the row cursor to the
  // SELECTION'S START row, the same "earlier" rule D8's own `y` uses -- not leave it wherever BROWSE
  // happened to be when the region was entered. Starts on the PERMISSION row (the list's last row,
  // BROWSE's own `j` moving it there before any region exists) and extends backward with `gg` (D5:
  // "gg/G place the caret... at the list's first/last selectable character", exercised without
  // `Selection.modify`/layout, so it works in jsdom) to the PROMPT row -- the selection's start is
  // now the prompt row, the opposite end from where BROWSE's cursor sat going in. A mutation that
  // makes `landCursorOnRowKey` a no-op in the `vquote` arm would leave `.row-current` on the
  // permission row (nothing else moves the BROWSE cursor during the region); the fix must move it to
  // the prompt row instead.
  it("D10: > moves the row cursor to the selection's start row, not wherever BROWSE was when the region started", () => {
    vi.useFakeTimers();
    try {
      const { container, root } = startedOnPromptRow();
      fireEvent.keyDown(root, { key: "j" }); // BROWSE: down onto the permission row (the list's last)
      expect(container.querySelector(".row-current")?.className).toContain("row-permission");
      fireEvent.keyDown(root, { key: "v" }); // CARET, entering on the permission row
      fireEvent.keyDown(root, { key: "v" }); // VISUAL
      fireEvent.keyDown(root, { key: "g" }); // gg: extend the CURSOR (never the anchor) back to the
      fireEvent.keyDown(root, { key: "g" }); //     list's first selectable character -- the prompt row
      fireEvent.keyDown(root, { key: ">", shiftKey: true });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("input");
      // The list (and its `.row-current`) is still on screen behind the composer.
      expect(container.querySelector(".row-current")?.className).toContain("row-prompt");
    } finally {
      vi.useRealTimers();
    }
  });

  it("> is refused (VISUAL stays) on an ended session, and while the draft is open in nvim", () => {
    const { container, root } = startedOnPromptRow();
    const modeOf = () => container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "l" });
    dispatch({ kind: "scratch", tab: 1, editing: true });
    posted.length = 0;
    fireEvent.keyDown(root, { key: ">", shiftKey: true });
    expect(container.querySelector(".band-message")?.textContent).toBe("the draft is open in nvim — finish there first");
    expect(modeOf()).toBe("visual");
    expect(container.querySelector("textarea")).toBeNull();
    expect(posted.filter((m) => m.type === "draft")).toHaveLength(0);
    dispatch({ kind: "scratch", tab: 1, editing: false });
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 3,
      throughRevision: 4,
      events: [{ type: "session_closed", reason: "provider exited" }],
    });
    // The region already ended with the session (the `[sessionEnded]` backstop). D2 allows entry on
    // an ended session, so re-enter: the refusal below is `>`'s own, not "there is no region".
    expect(modeOf()).toBe("browse");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "l" });
    expect(modeOf()).toBe("visual");
    posted.length = 0;
    fireEvent.keyDown(root, { key: ">", shiftKey: true });
    expect(container.querySelector(".band-message")?.textContent).toBe("this session has ended — nothing to quote into");
    expect(modeOf()).toBe("visual");
    expect(container.querySelector("textarea")).toBeNull();
    expect(posted.filter((m) => m.type === "draft")).toHaveLength(0);
  });

  it("a held > types nothing into the composer (D10: its repeats are swallowed until keyup)", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "l" });
    fireEvent.keyDown(root, { key: ">", shiftKey: true });
    const textarea = container.querySelector<HTMLTextAreaElement>("textarea")!;
    expect(document.activeElement).toBe(textarea);
    const afterFirstQuote = textarea.value;
    // The repeat lands where a real one would: on the focused box. jsdom never types a keydown into
    // a textarea, so the value alone proves nothing; what keeps the engine from typing `>` is the
    // keydown being default-prevented (fix round 2: the review's mutation M2 deleted the guard and
    // this test stayed green, because it fired on `root` and read only the value).
    const typed = fireEvent.keyDown(textarea, { key: ">", shiftKey: true, repeat: true });
    expect(typed).toBe(false);
    expect(textarea.value).toBe(afterFirstQuote);
    // Released and pressed again: an ordinary keystroke into the box, left to the engine.
    fireEvent.keyUp(textarea, { key: ">", shiftKey: true });
    expect(fireEvent.keyDown(textarea, { key: ">", shiftKey: true })).toBe(true);
  });

  // Spec §3 (tests, the mode machine): "the freeze held from CARET through VISUAL and back (a delta in
  // between leaves the reply's DOM untouched until the region ends)". Fix round 2: the review's
  // mutation M3 (thawing on vtoggle/vback) passed every test, since the D11 test above only does
  // `v` then Esc.
  it("D13: the freeze holds from CARET through VISUAL and back to CARET, a delta arriving at each stage", () => {
    const { container, root } = startedOnPromptRow();
    const modeOf = () => container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");
    const delta = (from: number, events: unknown[]) =>
      dispatch({ kind: "events", tab: 1, fromRevision: from, throughRevision: from + events.length, events });
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf()).toBe("caret");
    delta(3, [
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "one " },
    ]);
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    fireEvent.keyDown(root, { key: "v" });
    expect(modeOf()).toBe("visual");
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    delta(5, [{ type: "content_delta", turn_id: "t1", kind: "text", text: "two " }]);
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf()).toBe("caret");
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    delta(6, [{ type: "content_delta", turn_id: "t1", kind: "text", text: "three" }]);
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(0);
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf()).toBe("browse");
    expect(container.querySelectorAll(".row-assistant")).toHaveLength(1);
    expect(container.querySelector(".row-assistant")!.textContent).toContain("one two three");
  });

  // Spec §3: "idle Ctrl+c in CARET default-prevented" (D12, a consequence of D3). Fix round 2: the
  // review's mutation M7 (dropping the flash) survived; only keymap-level tests pinned it.
  it("D12: idle Ctrl+c in CARET is claimed and says why; in VISUAL it stays the engine's own copy", () => {
    const clipboard = stubClipboard();
    const { container, root } = startedOnPromptRow();
    const modeOf = () => container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");
    fireEvent.keyDown(root, { key: "v" });
    expect(fireEvent.keyDown(root, { key: "c", ctrlKey: true })).toBe(false);
    expect(container.querySelector(".band-message")?.textContent).toBe("nothing selected — v, then y");
    expect(modeOf()).toBe("caret");
    expect(clipboard.writeText).not.toHaveBeenCalled();
    fireEvent.keyDown(root, { key: "v" });
    // O6's default: VISUAL leaves idle Ctrl+c unclaimed, so the engine's native copy runs.
    expect(fireEvent.keyDown(root, { key: "c", ctrlKey: true })).toBe(true);
    expect(modeOf()).toBe("visual");
  });

  // D12: "A repeated ... Esc does nothing, and the key that leaves a mode swallows its own repeats
  // until keyup". Fix round 2 (review finding, minor): a held Esc stepped VISUAL back to CARET, then
  // its first auto-repeat ended the whole region.
  it("D12: a held Esc in VISUAL stops at CARET; only a fresh Esc ends the region", () => {
    const { container, root } = startedOnPromptRow();
    const modeOf = () => container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "l" });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf()).toBe("caret");
    fireEvent.keyDown(root, { key: "Escape", repeat: true });
    fireEvent.keyDown(root, { key: "Escape", repeat: true });
    expect(modeOf()).toBe("caret");
    expect(window.getSelection()?.toString()?.length).toBe(1);
    fireEvent.keyUp(root, { key: "Escape" });
    fireEvent.keyDown(root, { key: "Escape" });
    expect(modeOf()).toBe("browse");
  });

  // Fix round 2 (review finding, minor): the observer's flash named VISUAL while CARET was on.
  it("D13's observer ends CARET on a change under the caret, and names CARET", async () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" });
    const anchor = window.getSelection()!.anchorNode as Text;
    expect(anchor.nodeType).toBe(Node.TEXT_NODE);
    await act(async () => {
      anchor.data = "rewritten under the caret";
      await Promise.resolve();
    });
    expect(container.querySelector(".band-message")?.textContent).toBe("the conversation changed under it — CARET ended; v to start again");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
  });

  // Fix round 2 (review finding, D8): a region key announces ANY scroll of the list it caused, not only
  // the last nudge `scrollCaretIntoView` makes itself -- a j/k whose snap or probe revealed the line it
  // measured has already moved the list by then, and went unannounced, so `follow.ts` never learned a
  // `k` had scrolled up and the thawed list snapped back to the bottom when the region ended.
  it("D8: a motion that scrolled the list during the key announces it -- up when it went up", () => {
    const { container, root } = startedOnPromptRow();
    const list = container.querySelector<HTMLElement>(".message-list")!;
    let top = 100;
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => top,
      set: (v: number) => {
        top = v;
      },
    });
    const seen: string[] = [];
    list.addEventListener(USER_SCROLL_EVENT, (event) => seen.push((event as CustomEvent).detail));
    fireEvent.keyDown(root, { key: "v" });
    // A motion with nothing scrolled says nothing.
    fireEvent.keyDown(root, { key: "l" });
    expect(seen).toEqual([]);
    // The engine-side half of a step (a snap revealing the line it hit-tests) scrolls the list up.
    const proto = Selection.prototype as unknown as { modify: (a: string, d: string, g: string) => void };
    const stubbed = proto.modify;
    proto.modify = function (this: Selection, a: string, d: string, g: string) {
      top -= 20;
      stubbed.call(this, a, d, g);
    };
    try {
      fireEvent.keyDown(root, { key: "h" });
    } finally {
      proto.modify = stubbed;
    }
    expect(seen).toEqual(["up"]);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
  });

  /* v1 trial seam review, 2026-09-28 -- four findings against 3a's own merge. Each gets a test here
     (finding 1, 3, 4) or in the "App refused commands" describe below (finding 2); finding 5 is a
     dated-record blank-line fix with nothing to unit-test. */

  /** Finding 1: `slashReply`'s own effect (`App.tsx`'s `covered` check) listed every OTHER overlay
   *  a bare /model or /effort reply must not open a picker over -- the chooser, `?`, the `/` prompt
   *  -- but not the region. A reply landing during CARET/VISUAL/V-LINE opened `SlashPicker`, which
   *  focuses itself (`slashPickerFocusRequest`), and the root's own `onFocus` (D12/D13's backstop:
   *  "focus landing on anything but the root ends the region") then called `exitRegion()` -- so the
   *  very next `y` copied nothing and the very next `Enter` sent "/model haiku" into the picker
   *  instead of doing whatever the user meant inside the region. Fixed the same way the chooser/`?`/
   *  `/`-prompt cases already were: `isRegionMode(modeRef.current)` joins the `covered` disjunction,
   *  so the reply stays plain transcript text and neither the picker nor the focus effect ever run. */
  it("finding 1: a bare /model's reply landing during VISUAL neither opens the picker nor ends the region", () => {
    const MODEL_REPLY =
      "Current model: `Haiku 4.5` (effort: high)\n" +
      "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
      "fable[1m], opusplan, default, or a full model ID.";
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "i" });
    const textarea = container.querySelector<HTMLTextAreaElement>("textarea")!;
    fireEvent.change(textarea, { target: { value: "/model" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(lastOfType("send_message")!.text).toBe("/model");
    fireEvent.keyDown(textarea, { key: "Escape" }); // back to BROWSE, nothing left in the box
    fireEvent.keyDown(root, { key: "g" }); // gg (already on the prompt row; the probe's own sequence)
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" }); // charwise inclusive: "he" (D3/D4)
    expect(window.getSelection()?.toString()).toBe("he");
    // The reply to that bare /model, arriving while VISUAL holds the keys.
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 3,
      throughRevision: 5,
      events: [
        { type: "turn_started", turn_id: "t1" },
        { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
      ],
    });
    expect(container.querySelector(".slash-picker"), "no picker opened over the region").toBeNull();
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
    expect(window.getSelection()?.toString(), "the selection the region built is still there").toBe("he");
    // And the region still works normally afterwards: y still copies "he", not nothing.
    const clipboard = stubClipboard();
    fireEvent.keyDown(root, { key: "y" });
    expect(clipboard.writeText).toHaveBeenLastCalledWith("he");
  });

  /** Finding 2: `chooseSlashOption` sends the picker's choice through the same `sendMessage` an
   *  ordinary composer send uses, which records `{kind: "send", text}` -- so a refusal restores
   *  `${record.text}\n${typedSince}` into the box, on the theory that `record.text` really did
   *  leave it (the ordinary case, spec P2-A3's own "R1"). A picker choice never went through the
   *  box at all: `chooseSlashOption` calls `sendMessage` directly. Reproduced exactly as found --
   *  quote "hello world" into an EMPTY draft first (`V >`, D10), so the box holds `"> hello
   *  world\n\n"` and nothing else has touched it; only THEN does the earlier bare `/model`'s reply
   *  open the picker (the picker only reaches BROWSE-blocking focus once `mode` is back to "input",
   *  which the quote's own D10 exit already restored) and only THEN is a picker choice refused.
   *  Fixed: a dedicated in-flight kind (`"picker-send"`) whose refusal is a footer flash alone --
   *  the draft (whatever it holds) is left exactly as the user last set it. */
  it("finding 2: a refused picker choice flashes and leaves an already-quoted draft untouched", () => {
    vi.useFakeTimers();
    try {
      const MODEL_REPLY =
        "Current model: `Haiku 4.5` (effort: high)\n" +
        "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
        "fable[1m], opusplan, default, or a full model ID.";
      const { container, root } = startedOnPromptRow();
      // The bare /model, sent first from an empty box.
      fireEvent.keyDown(root, { key: "i" });
      const firstBox = container.querySelector<HTMLTextAreaElement>("textarea")!;
      fireEvent.change(firstBox, { target: { value: "/model" } });
      fireEvent.keyDown(firstBox, { key: "Enter" });
      expect(lastOfType("send_message")!.text).toBe("/model");
      fireEvent.keyDown(firstBox, { key: "Escape" }); // back to BROWSE, box now empty
      // V > : quote "hello world" into the (empty) draft (D10).
      fireEvent.keyDown(root, { key: "V", shiftKey: true }); // V-LINE directly (O8)
      fireEvent.keyDown(root, { key: ">", shiftKey: true });
      expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("input");
      expect(container.querySelector<HTMLTextAreaElement>("textarea")!.value).toBe("> hello world\n\n");
      act(() => vi.advanceTimersByTime(300)); // flush the quote's own mirrorDraft debounce
      expect(lastOfType("draft")).toMatchObject({ tab: 1, text: "> hello world\n\n" });
      const draftPostsBefore = posted.filter((m) => m.type === "draft").length;
      // Now the /model reply lands. Opening the picker steals focus from the composer's textarea
      // (`SlashPicker`'s own mount effect), and the textarea's `onBlur` ("click away", unrelated
      // and pre-existing -- `Composer.tsx:371-375`) reports BROWSE -- this test asserts nothing
      // about `mode` either way; what matters is only what happens to the draft itself.
      dispatch({
        kind: "events",
        tab: 1,
        fromRevision: 3,
        throughRevision: 5,
        events: [
          { type: "turn_started", turn_id: "t1" },
          { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: MODEL_REPLY, stop_reason: null, usage: null },
        ],
      });
      const picker = container.querySelector<HTMLElement>(".slash-picker")!;
      expect(picker, "the reply parsed and opened a picker").not.toBeNull();
      fireEvent.keyDown(picker, { key: "Enter" }); // chooses the marked-current option, "haiku"
      const pickerRequest = lastOfType("send_message")!;
      expect(pickerRequest.text).toBe("/model haiku");
      dispatch({ kind: "command_result", requestId: pickerRequest.request_id, ok: false, error: "no active session" });
      // Fixed: the picker's own refusal never mirrors anything back to Rust -- no new `draft` post
      // at all, so the quoted text is left exactly as it was, never restored, still less
      // concatenated with "/model haiku" ahead of it.
      act(() => vi.advanceTimersByTime(300));
      expect(posted.filter((m) => m.type === "draft")).toHaveLength(draftPostsBefore);
      // A footer flash, not the banner "It is back in the box" message an ordinary composer send's
      // refusal gets.
      expect(container.querySelector(".band-message")?.textContent).toBe("no active session");
      expect(container.querySelector(".command-notice")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  /** Finding 3: `resolveRegionModifierKey` (shared by CARET and VISUAL/V-LINE) used to swallow every
   *  Ctrl chord, Ctrl+e/Ctrl+y included, with no feedback at all -- even though the `?` overlay and
   *  CARET_KEYS' own `?` row both claimed "any other key leaves"/"ends the caret too". vim scrolls
   *  on these keys in Visual mode; fixed the same way, reusing BROWSE's own one-line `scroll-line`
   *  step (`keymap.test.ts` pins the low-level `resolveKey` mapping; this is the DOM-level proof that
   *  `App.tsx`'s region switch actually scrolls the list and leaves the caret/selection and the mode
   *  alone). */
  it("finding 3: Ctrl+e/Ctrl+y scroll the frozen list one line in CARET, without moving the caret or ending the region", () => {
    const { container, root } = startedOnPromptRow();
    const list = container.querySelector<HTMLElement>(".message-list")!;
    list.scrollTop = 100;
    fireEvent.keyDown(root, { key: "v" }); // CARET, on "hello world"'s first character
    const before = window.getSelection()!.toString();
    fireEvent.keyDown(root, { key: "e", ctrlKey: true });
    expect(list.scrollTop, "Ctrl+e scrolled down").toBeGreaterThan(100);
    expect(window.getSelection()!.toString(), "the caret did not move").toBe(before);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    const afterCtrlE = list.scrollTop;
    fireEvent.keyDown(root, { key: "y", ctrlKey: true });
    expect(list.scrollTop, "Ctrl+y scrolled back up").toBeLessThan(afterCtrlE);
    expect(window.getSelection()!.toString()).toBe(before);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
  });

  it("finding 3: Ctrl+e/Ctrl+y scroll the frozen list in VISUAL too, without touching the highlighted selection", () => {
    const { container, root } = startedOnPromptRow();
    const list = container.querySelector<HTMLElement>(".message-list")!;
    list.scrollTop = 100;
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "l" }); // "he"
    expect(window.getSelection()?.toString()).toBe("he");
    fireEvent.keyDown(root, { key: "e", ctrlKey: true });
    expect(list.scrollTop).toBeGreaterThan(100);
    expect(window.getSelection()?.toString(), "the highlighted selection did not change").toBe("he");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("visual");
  });

  it("finding 3: a count repeats Ctrl+e/Ctrl+y in the region, the same as R4's other counted region motions", () => {
    const { container, root } = startedOnPromptRow();
    const list = container.querySelector<HTMLElement>(".message-list")!;
    list.scrollTop = 1000;
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "e", ctrlKey: true }); // one line down, for a baseline step size
    const oneLine = list.scrollTop - 1000;
    expect(oneLine).toBeGreaterThan(0);
    list.scrollTop = 1000;
    fireEvent.keyDown(root, { key: "3" }); // count
    fireEvent.keyDown(root, { key: "e", ctrlKey: true });
    expect(list.scrollTop).toBeCloseTo(1000 + oneLine * 3, 1);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
  });

  /** Finding 4: the `vend` flash for an unbound Enter said "Enter is not a VISUAL key (y copies)" --
   *  true, but useless: it never pointed at the one route that actually reaches a folded item-7 row
   *  from the region (D7: "Text not drawn is not reachable... `Enter` before `v` unfolds a fold or a
   *  run"). Enter now gets its own short flash naming that route, in both CARET and VISUAL/V-LINE;
   *  every other unbound key (`d`, pinned above) keeps its existing wording unchanged. */
  it("finding 4: Enter's vend flash points at BROWSE's own unfold-then-v route, in CARET and in VISUAL", () => {
    const { container, root } = startedOnPromptRow();
    fireEvent.keyDown(root, { key: "v" }); // CARET
    fireEvent.keyDown(root, { key: "Enter" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    const caretFlash = container.querySelector(".band-message")?.textContent ?? "";
    expect(caretFlash).toContain("Enter");
    expect(caretFlash, "names BROWSE's Enter-then-v route").toContain("BROWSE");
    expect(caretFlash).toContain("v");

    fireEvent.keyDown(root, { key: "v" }); // CARET again
    fireEvent.keyDown(root, { key: "v" }); // VISUAL
    fireEvent.keyDown(root, { key: "Enter" });
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    const visualFlash = container.querySelector(".band-message")?.textContent ?? "";
    expect(visualFlash).toContain("Enter");
    expect(visualFlash, "names BROWSE's Enter-then-v route").toContain("BROWSE");
    expect(visualFlash).toContain("v");
    // The unchanged case (`d`) still reads exactly as it did before this fix.
    fireEvent.keyDown(root, { key: "v" });
    fireEvent.keyDown(root, { key: "d" });
    expect(container.querySelector(".band-message")?.textContent).toBe("CARET ended: d is not a CARET key (v selects)");
  });
});

/* K04 (2026-09-29, arrival #22/K04/K07): a failed-start tab's `r` was dead -- not on arrival (the kbux
   S13 evidence: an `r` right after a `Ctrl+l` worked), but after the tab failed while its composer had
   the keys. The textarea went disabled, WebKit's focus fix-up moved focus to <body>, and the document
   replay re-dispatched the key on `.agent-ui-root`, the empty tab's PARENT, whose only handler is the
   `?`/confirm capture; the replay also focused that root, so every later key was dead too. jsdom does
   not apply WebKit's fix-up (a disabled focused textarea stays `activeElement`), so these tests blur
   explicitly where WebKit would. */
describe("K04: r on a failed empty tab", () => {
  const FAILED = { ...LIVE_TAB, state: "failed", failure: "claude: not logged in -- run claude /login" } as const;

  /** A starting tab whose composer holds the keys (C1: it is live while starting), then failing. */
  function failWhileTyping() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    // The panel holds the keys (the band names BROWSE/INPUT only then).
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    const textarea = rendered.container.querySelector("textarea")!;
    expect(document.activeElement).toBe(textarea);
    dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
    return rendered;
  }

  /** WebKit's focus fix-up, done by hand: a focused control that went disabled loses focus to <body>
   *  and fires `blur` (jsdom's own `blur()` refuses a disabled element, so the event is fired and the
   *  unmount that follows it moves jsdom's focus to <body>); anything else focused is blurred. */
  function focusFixup() {
    const el = document.activeElement as HTMLElement;
    if (el instanceof HTMLTextAreaElement && el.disabled) fireEvent.blur(el);
    else act(() => el.blur());
  }

  it("K04-a (control): after an arrival, r on the failed tab posts reset_tab", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
    dispatch({ kind: "arrive" });
    const target = (document.activeElement ?? document.body) as HTMLElement;
    expect(container.querySelector(".empty-tab")!.contains(target)).toBe(true);
    fireEvent.keyDown(target, { key: "r" });
    expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });
  });

  it("K04-b: failing while the composer had the keys, r on <body> still resets the tab", () => {
    failWhileTyping();
    // WebKit's focus fix-up: the focused control went disabled (or, after the fix, the keys already
    // moved to the empty tab's root); either way this is where a key lands in the reported trap.
    focusFixup();
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "r" });
    expect(posted.filter((m) => m.type === "reset_tab")).toHaveLength(1);
  });

  it("K04-c: the keys are not stranded afterwards -- a second r still resets", () => {
    failWhileTyping();
    focusFixup();
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "r" });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "r" });
    expect(posted.filter((m) => m.type === "reset_tab")).toHaveLength(2);
  });

  it("K04-d: a failed tab with no conversation is the empty tab; a kept one is the conversation, and r on <body> resets it", () => {
    const { container, unmount } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    unmount();
    posted = [];
    const kept = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "restored reply" }] }));
    const message = "the connection to the provider ended before this session did (x)";
    dispatch({ kind: "error", tab: 1, message });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "failed", failure: message }] });
    expect(kept.container.querySelector(".agent-ui-conversation")).not.toBeNull();
    act(() => (document.activeElement as HTMLElement | null)?.blur());
    fireEvent.keyDown(document.body, { key: "r" });
    expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });
  });

  it("K04-e: the tab failing moves the keys from the dead composer to the empty tab's own root, in BROWSE", () => {
    const { container } = failWhileTyping();
    const emptyTab = container.querySelector(".empty-tab")!;
    expect(document.activeElement).toBe(emptyTab);
    expect(container.querySelector(".status-band")!.textContent).toContain("BROWSE");
    expect(container.querySelector("textarea")).toBeNull();
    fireEvent.keyDown(document.activeElement!, { key: "r" });
    expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });
  });

  it("K04-f: a key replayed off <body> on the empty layout reaches the empty tab (a Dismiss that removed its own banner)", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
      dispatch({ kind: "command_result", requestId: "req-unknown", ok: false, error: "could not do that" });
      const dismiss = buttonLabelled(container, "Dismiss")!;
      act(() => dismiss.focus());
      fireEvent.click(dismiss);
      expect(container.querySelector(".command-notice")).toBeNull();
      expect(document.activeElement).toBe(document.body);
      fireEvent.keyDown(document.body, { key: "r" });
      expect(lastOfType("reset_tab")).toMatchObject({ tab: 1 });

      // And on a not-started tab, the dashboard's own `w` (deferred by the typing guard).
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 2));
      dispatch({ kind: "command_result", requestId: "req-unknown-2", ok: false, error: "again" });
      const dismiss2 = buttonLabelled(container, "Dismiss")!;
      act(() => dismiss2.focus());
      fireEvent.click(dismiss2);
      expect(document.activeElement).toBe(document.body);
      fireEvent.keyDown(document.body, { key: "w" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 2));
      expect(lastOfType("tab_verb")).toMatchObject({ verb: "choose" });
    } finally {
      vi.useRealTimers();
    }
  });

  it("K04-g: failing while the chooser is open over the empty tab leaves the keys with the chooser", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    dispatch({ kind: "enter_input" });
    dispatch({ kind: "chooser", open: [], records: [] });
    const chooser = container.querySelector<HTMLElement>(".chooser")!;
    expect(chooser.contains(document.activeElement)).toBe(true);
    dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
    expect(chooser.contains(document.activeElement)).toBe(true);
  });

  it("K04 fix round: a key replayed off <body> never reaches the empty tab while the chooser is open", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
    dispatch({ kind: "chooser", open: [], records: [] });
    const chooser = container.querySelector<HTMLElement>(".chooser")!;
    expect(chooser.contains(document.activeElement)).toBe(true);
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "r" });
    expect(lastOfType("reset_tab")).toBeUndefined();
    expect(container.querySelector(".chooser")).not.toBeNull();
    expect(container.querySelector(".chooser")!.contains(document.activeElement)).toBe(true);
  });

  it("K04 edge: r twice on a failed tab resets once per press, and the reset dashboard's own r does not resume by accident", () => {
    vi.useFakeTimers();
    try {
      const resumable: Hello = {
        ...HELLO,
        backend: "sidecar",
        resumableSessions: [{ provider: "claude", providerSessionId: "newest", createdAt: "", updatedAt: "" }],
      };
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...resumable });
      dispatch({ kind: "tabs", active: 1, tabs: [FAILED] });
      dispatch({ kind: "arrive" });
      fireEvent.keyDown(document.activeElement!, { key: "r" });
      // Rust answers the reset: the tab is not_started again, the dashboard is back.
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
      expect(container.querySelector(".empty-tab")).not.toBeNull();
      // The second, fast `r` lands on the dashboard, where `r` means "resume newest" -- deferred,
      // and cancelled by nothing here, so it must be a deliberate lone key to act: a repeat never is.
      fireEvent.keyDown(document.activeElement!, { key: "r", repeat: true });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 2));
      expect(posted.filter((m) => m.type === "reset_tab")).toHaveLength(1);
      expect(lastOfType("resume")).toBeUndefined();
      // A fast second `r` that is NOT a repeat (review, fix round 1): typed within the guard's window
      // after the first, it is refused (`TypingGuard.mayAnswerNow`), never a resume.
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      fireEvent.keyDown(document.activeElement!, { key: "x" });
      act(() => vi.advanceTimersByTime(50));
      fireEvent.keyDown(document.activeElement!, { key: "r" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(lastOfType("resume")).toBeUndefined();
      // The control: the same `r` standing alone does resume, so the refusal above is the guard's.
      fireEvent.keyDown(document.activeElement!, { key: "r" });
      act(() => vi.advanceTimersByTime(TYPING_GUARD_MS * 4));
      expect(lastOfType("resume")).toMatchObject({ provider_session_id: "newest" });
    } finally {
      vi.useRealTimers();
    }
  });
});

/* #22 (owner decision, 2026-09-29: "contrl h之后再contrl l，会自动跳到最底下，能不能类似记住光标位置"): coming back
   to the panel (`Ctrl+h/l`, `prefix a`, a tray chip) restores a reader's cursor row and scroll unless they
   were following the bottom, who land on the last row and keep following; a waiting card still wins (P1),
   and a new tab still lands INPUT. The geometry below is a fake layout: rows stacked at given heights in a
   list `viewport` px tall at y = 0, `scrollTop` clamped as a browser's is, every rect following it.
   `scrollIntoView` is this file's mock, so a reveal of a row that fits moves nothing; the view is set by
   writing `scrollTop`. None of this proves what a real WebKit draws. */
describe("#22: coming back restores where you were", () => {
  let revision = 0;
  const row = (n: number) => ({ seq: n, text: `row ${n}` });
  function started(n = 5, extra: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    // Every `seq` in a snapshot is below its `throughRevision` (the reducer's next `seq`), so rows that
    // stream in afterwards sort after these.
    revision = 100;
    dispatchLiveTab(
      snapshotState({ activeTurnId: "t", transcript: Array.from({ length: n }, (_, i) => row(i + 1)), ...extra }),
      revision,
    );
    dispatch({ kind: "pane_focus", focused: true });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + list.length, events: list });
    revision += list.length;
  }
  /** Replies streamed in: one new row per text. */
  function stream(...texts: string[]) {
    events(
      ...texts.flatMap((text): AgentDomainEvent[] => [
        { type: "assistant_message_boundary", turn_id: "t" },
        { type: "content_delta", turn_id: "t", kind: "text", text },
      ]),
    );
  }
  const listOf = (c: HTMLElement) => c.querySelector<HTMLElement>(".message-list")!;
  const rootOf = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const rowsOf = (c: HTMLElement) => Array.from(listOf(c).querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
  const current = (c: HTMLElement) => (c.querySelector(".row-current .row-body")?.textContent ?? "").trim();
  const modeOf = (c: HTMLElement) => c.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode;
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });

  /** Lays the list out (again): `scrollTop` starts where given, so a re-layout after rows arrived keeps
   *  the view where it was, as a browser's would. A row that arrives after this call counts 300px
   *  towards `scrollHeight` at once (fix round): in a browser the list is taller by the time
   *  `MessageList`'s layout effect reads it, and a stale height would read as "at the bottom". */
  function layOut(c: HTMLElement, heights: number[], scrollTop = 0, viewport = 400) {
    const list = listOf(c);
    const rows = rowsOf(c);
    expect(rows.length).toBe(heights.length);
    const totalNow = () => rowsOf(c).reduce((sum, _r, i) => sum + (heights[i] ?? 300), 0);
    const clamp = (v: number) => Math.max(0, Math.min(totalNow() - viewport, v));
    let top = clamp(scrollTop);
    Object.defineProperty(list, "clientHeight", { value: viewport, configurable: true });
    Object.defineProperty(list, "scrollHeight", { get: totalNow, configurable: true });
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => top,
      set: (v: number) => {
        top = clamp(v);
      },
    });
    list.getBoundingClientRect = () => ({ top: 0, bottom: viewport }) as DOMRect;
    let y = 0;
    rows.forEach((r, i) => {
      const at = y;
      y += heights[i];
      r.getBoundingClientRect = () => ({ top: at - top, bottom: at + heights[i] - top }) as DOMRect;
      r.querySelector<HTMLElement>(".row-body")!.style.lineHeight = "20px";
    });
    return list;
  }
  const FIVE = [300, 300, 300, 300, 300];
  const SEVEN = [300, 300, 300, 300, 300, 300, 300];

  /** "Parked on row 3": at the bottom (R1 has the cursor on row 5), the reader scrolls up to 600 (R1's
   *  clamp moves the cursor to row 4, the nearest visible), then `k` (row 3 fits and is on screen: the
   *  mocked reveal moves nothing, and `k` stops following). */
  function parkOnRow3(c: HTMLElement) {
    const list = layOut(c, FIVE, 1100);
    act(() => rootOf(c).focus());
    expect(current(c)).toBe("row 5");
    list.scrollTop = 600;
    fireEvent.scroll(list);
    expect(current(c)).toBe("row 4");
    press("k");
    expect(current(c)).toBe("row 3");
    expect(list.scrollTop).toBe(600);
    return list;
  }
  const leave = () => dispatch({ kind: "pane_focus", focused: false });
  type Order = "focus-first" | "arrive-first";
  function come(order: Order = "focus-first") {
    if (order === "focus-first") {
      dispatch({ kind: "pane_focus", focused: true });
      dispatch({ kind: "arrive" });
    } else {
      dispatch({ kind: "arrive" });
      dispatch({ kind: "pane_focus", focused: true });
    }
  }
  /** What reached the list: `resume` (following re-armed) and `user <direction>` announcements. */
  function recording<T>(run: () => T): { seen: string[]; result: T } {
    const seen: string[] = [];
    const onResume = () => seen.push("resume");
    document.addEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    try {
      return { seen, result: run() };
    } finally {
      document.removeEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    }
  }

  it("T1: a reader following the bottom lands on the last row and keeps following", () => {
    const { container } = started();
    layOut(container, FIVE, 1100);
    expect(current(container)).toBe("row 5");
    leave();
    stream("row 6", "row 7");
    layOut(container, SEVEN, 2100 - 400);
    const { seen } = recording(() => come());
    expect(current(container)).toBe("row 7");
    expect(seen).toEqual(["resume"]);
    expect(modeOf(container)).toBe("browse");
  });

  it.each<Order>(["focus-first", "arrive-first"])("T2: a reader who had scrolled up gets their row and scroll back (%s)", (order) => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    stream("row 6", "row 7");
    // Hidden and shown again, WebKit may lay the list out from the top: no scroll event, no clamp.
    const list = layOut(container, SEVEN, 0);
    const { seen } = recording(() => come(order));
    expect(current(container)).toBe("row 3");
    expect(list.scrollTop).toBe(600);
    expect(seen).toEqual([]);
    expect(modeOf(container)).toBe("browse");
  });

  it("T2b: at the bottom with the cursor above the last row is not following: row and view stay, streaming does not carry them", () => {
    const { container } = started();
    const first = layOut(container, FIVE, 1100);
    act(() => rootOf(container).focus());
    press("k");
    expect(current(container)).toBe("row 4");
    expect(first.scrollTop).toBe(1100);
    leave();
    stream("row 6", "row 7");
    const list = layOut(container, SEVEN, 1100);
    const { seen } = recording(() => {
      come();
      stream("row 8");
      layOut(container, [...SEVEN, 300], 1100);
    });
    expect(current(container)).toBe("row 4");
    expect(list.scrollTop).toBe(1100);
    expect(seen).toEqual([]);
  });

  it("T3: a scroll the reader did not make while away (R1's clamp moved the cursor) is undone", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    stream("row 6", "row 7");
    const list = layOut(container, SEVEN, 600);
    list.scrollTop = 1500;
    fireEvent.scroll(list);
    expect(current(container)).toBe("row 6");
    come();
    expect(current(container)).toBe("row 3");
    expect(list.scrollTop).toBe(600);
  });

  it("T4: a row removed above the cursor while away: the same row comes back, by key", () => {
    const { container } = started(4, {
      transcript: [row(1), row(2), row(4), row(5)],
      pendingPermissions: [{ seq: 3, permissionId: "p3", toolUseId: null, toolName: "Write", input: { file_path: "a" } }],
    });
    layOut(container, FIVE, 1100);
    act(() => rootOf(container).focus());
    expect(current(container)).toBe("row 5");
    press("k");
    expect(current(container)).toBe("row 4");
    leave();
    events({ type: "permission_resolved", permission_id: "p3", outcome: "allowed" });
    expect(container.querySelector(".row-permission")).toBeNull();
    const list = layOut(container, [300, 300, 300, 300], 1100);
    come();
    expect(current(container)).toBe("row 4");
    expect(list.scrollTop).toBe(800);
  });

  it("T5: a card that arrived while away wins, and the park is dropped with it", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    events({ type: "permission_requested", permission_id: "p9", tool_use_id: "toolu_9", tool_name: "Bash", input: {} });
    layOut(container, [...FIVE, 300], 600);
    come();
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    expect(modeOf(container)).toBe("browse");
    // The park is gone, not merely outranked by the card: with the card resolved, an arrive with no
    // leave in between restores nothing.
    events({ type: "permission_resolved", permission_id: "p9", outcome: "allowed" });
    layOut(container, FIVE, 600);
    const before = current(container);
    expect(before).not.toBe("row 3");
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe(before);
  });

  it("T6: a new tab still lands INPUT; the switch back is Task 9's restore, and an arrive right after moves nothing", () => {
    const { container } = started();
    parkOnRow3(container);
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new", state: "not_started" }];
    act(() => {
      window.__neovibeDispatch!(JSON.stringify({ kind: "tabs", active: 2, tabs: two }));
      window.__neovibeDispatch!(JSON.stringify({ kind: "enter_input" }));
    });
    const box = container.querySelector("textarea");
    expect(document.activeElement).toBe(box);
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({
      kind: "snapshot", tab: 1, throughRevision: revision,
      state: snapshotState({ activeTurnId: "t", transcript: [row(1), row(2), row(3), row(4), row(5)] }),
    });
    expect(current(container)).toBe("row 3");
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe("row 3");
  });

  /* Every arrival route reduces to the same envelopes (read, not run): `Ctrl+h`/`Ctrl+l` (`move_focus`;
     the agent host's capture controller hands them to GTK before the page sees a keydown) and `prefix a`
     with the chat shown send `pane_focus` and `arrive`; `prefix a`/a tray chip with the chat hidden first
     hid it with the keys in it, which reports the agent unfocused (`ModuleGrid::hide_module` moves the
     keys first). GTK's focus notify and `arrive` are not ordered by contract, so both orders. */
  it.each<[string, Order]>([
    ["Ctrl+h then Ctrl+l", "focus-first"],
    ["Ctrl+h then Ctrl+l", "arrive-first"],
    ["prefix a, chat shown", "focus-first"],
    ["prefix a, chat shown", "arrive-first"],
    ["prefix a / tray chip, chat hidden", "focus-first"],
    ["prefix a / tray chip, chat hidden", "arrive-first"],
  ])("T7: %s (%s) restores the parked row and scroll", (_route, order) => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    stream("row 6", "row 7");
    const list = layOut(container, SEVEN, 0);
    come(order);
    expect(current(container)).toBe("row 3");
    expect(list.scrollTop).toBe(600);
  });

  it("T7b: the card route (a tray chip agent ⚑N, prefix a with a card) lands on the card, and the park is gone", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    events({ type: "permission_requested", permission_id: "p9", tool_use_id: "toolu_9", tool_name: "Bash", input: {} });
    layOut(container, [...FIVE, 300], 600);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    events({ type: "permission_resolved", permission_id: "p9", outcome: "allowed" });
    layOut(container, FIVE, 600);
    const before = current(container);
    expect(before).not.toBe("row 3");
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe(before);
  });

  it("T8: the reader scrolled the list with the wheel while away: that is where they are", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    stream("row 6", "row 7");
    const list = layOut(container, SEVEN, 600);
    fireEvent.wheel(list, { deltaY: 300 });
    list.scrollTop = 1200;
    fireEvent.scroll(list);
    expect(current(container)).toBe("row 5");
    come();
    expect(current(container)).toBe("row 5");
    expect(list.scrollTop).toBe(1200);
  });

  it("T9: a panel key since the leave makes the park stale", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    act(() => rootOf(container).focus());
    press("j");
    expect(current(container)).toBe("row 4");
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe("row 4");
  });

  it("T10: a tab switch while away drops the park: the other tab lands by the live rule", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [row(1), row(2)] }) });
    // The same list element now holds tab 2's two rows, followed to its end (it opened there).
    layOut(container, [300, 300], 200);
    const { seen } = recording(() => dispatch({ kind: "arrive" }));
    expect(current(container)).toBe("row 2");
    expect(seen).toEqual(["resume"]);
  });

  it("T11: a reader who left from INPUT comes back in BROWSE on their row, draft untouched", () => {
    const { container } = started();
    const list = parkOnRow3(container);
    press("i");
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "half a thought" } });
    leave();
    layOut(container, FIVE, 0);
    come();
    expect(modeOf(container)).toBe("browse");
    expect(current(container)).toBe("row 3");
    expect(list.scrollTop).toBe(600);
    act(() => rootOf(container).focus());
    press("i");
    expect(container.querySelector("textarea")!.value).toBe("half a thought");
  });

  it("T12: in a conversation that fits the view, a cursor on row 1 stays there, and streaming does not move it", () => {
    const { container } = started(3);
    layOut(container, [100, 100, 100]);
    act(() => rootOf(container).focus());
    press("g");
    press("g");
    expect(current(container)).toBe("row 1");
    leave();
    const { seen } = recording(() => come());
    expect(current(container)).toBe("row 1");
    expect(seen).toEqual([]);
    stream("row 4");
    layOut(container, [100, 100, 100, 100]);
    expect(current(container)).toBe("row 1");
  });

  it("T13: the restored row is off screen after a reflow: it is revealed, and the cursor stays on it", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    stream("row 6", "row 7");
    layOut(container, [700, 700, 300, 300, 300, 300, 300], 600);
    const reveal = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    reveal.mockClear();
    come();
    expect(current(container)).toBe("row 3");
    expect(reveal.mock.contexts).toContain(rowsOf(container)[2]);
  });

  it("fix round, Codex 1: a parked reader stays parked even if the list re-armed following while away", () => {
    const { container } = started(4, {
      transcript: [row(1), row(2), row(4), row(5)],
      pendingPermissions: [{ seq: 3, permissionId: "p3", toolUseId: null, toolName: "Write", input: { file_path: "a" } }],
    });
    layOut(container, FIVE, 1100);
    act(() => rootOf(container).focus());
    press("k");
    expect(current(container)).toBe("row 4");
    leave();
    // The card above resolves: the list shrinks, the browser clamps 1100 -> 800 and says so with a
    // scroll event, which `MessageList` reads as "at the bottom": following again.
    events({ type: "permission_resolved", permission_id: "p3", outcome: "allowed" });
    const list = layOut(container, [300, 300, 300, 300], 1100);
    fireEvent.scroll(list);
    come();
    expect(current(container)).toBe("row 4");
    // The next streamed row must not carry the view (and, through R1's clamp, the cursor) away.
    stream("row 6");
    layOut(container, [300, 300, 300, 300, 300], list.scrollTop);
    stream("row 7");
    expect(list.scrollTop).toBe(800);
    fireEvent.scroll(list);
    expect(current(container)).toBe("row 4");
  });

  it("fix round: a park that was following lands last even when the view is not at the bottom on arrival", () => {
    const { container } = started();
    layOut(container, FIVE, 1100);
    expect(current(container)).toBe("row 5");
    leave();
    stream("row 6");
    layOut(container, [...FIVE, 300], 0); // shown again from the top: no scroll event
    const { seen } = recording(() => come());
    expect(current(container)).toBe("row 6");
    expect(seen).toEqual(["resume"]);
  });

  /* Every way the park is dropped or consumed, each made observable the same way: after it, a scroll
     the reader did not make moves the cursor (R1's clamp), and the arrival must leave it there rather
     than restore row 3. */
  function driftAway(c: HTMLElement): string {
    const list = listOf(c);
    list.scrollTop = 1100;
    fireEvent.scroll(list);
    const now = current(c);
    expect(now).not.toBe("row 3");
    return now;
  }
  it.each<[string, (c: HTMLElement) => void]>([
    ["a touch drag on the list", (c) => fireEvent.touchMove(listOf(c))],
    ["a pointer press in the panel", (c) => fireEvent.pointerDown(rootOf(c))],
    ["enter_input", () => dispatch({ kind: "enter_input" })],
    ["an arrival that consumed it", () => come()],
    ["a tab switch and back", (c) => {
      const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
      dispatch({ kind: "tabs", active: 2, tabs: two });
      dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState({ transcript: [row(1)] }) });
      dispatch({ kind: "tabs", active: 1, tabs: two });
      dispatch({
        kind: "snapshot", tab: 1, throughRevision: revision,
        state: snapshotState({ activeTurnId: "t", transcript: [row(1), row(2), row(3), row(4), row(5)] }),
      });
      layOut(c, FIVE, 600); // the rows are new elements: lay them out again
    }],
  ])("fix round: %s drops the park", (_name, drop) => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    drop(container);
    const before = driftAway(container);
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe(before);
  });
  it("fix round: focus_permission drops the park (the card resolved before the next arrive)", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    events({ type: "permission_requested", permission_id: "p9", tool_use_id: "toolu_9", tool_name: "Bash", input: {} });
    layOut(container, [...FIVE, 300], 600);
    dispatch({ kind: "focus_permission", tab: 1 });
    events({ type: "permission_resolved", permission_id: "p9", outcome: "allowed" });
    layOut(container, FIVE, 600);
    const before = driftAway(container);
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe(before);
  });
  it("fix round: a leave from the empty layout clears an older park", () => {
    const { container } = started();
    parkOnRow3(container);
    leave();
    // The tab was reset while away and started again: same tab, no switch.
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "not_started" }] });
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    dispatch({ kind: "pane_focus", focused: true });
    leave();
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({
      kind: "snapshot", tab: 1, throughRevision: revision,
      state: snapshotState({ activeTurnId: "t", transcript: [row(1), row(2), row(3), row(4), row(5)] }),
    });
    layOut(container, FIVE, 0);
    const before = driftAway(container);
    dispatch({ kind: "arrive" });
    expect(current(container)).toBe(before);
  });

  it("T14: a launch arrival (nothing parked) lands on the last row, following", () => {
    const { container } = started();
    const { seen } = recording(() => dispatch({ kind: "arrive" }));
    expect(current(container)).toBe("row 5");
    expect(seen).toEqual(["resume"]);
  });
});
