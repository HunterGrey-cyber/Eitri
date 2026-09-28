// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App, { accumulateMotionCount, HINT_PENDING_TIMEOUT_MS, MAX_MOTION_COUNT, statusWarning } from "./App";
import { initialState } from "./reducer";
import { RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT } from "./follow";
import type { AgentDomainEvent, AgentUiState, Hello, ProviderInfo } from "./types";
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

  it("with no card: BROWSE on the last row, following resumed, no textarea focused", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState({ transcript: [{ seq: 1, text: "first" }, { seq: 2, text: "second" }] }), 2);
    dispatch({ kind: "pane_focus", focused: true });
    const root = container.querySelector(".agent-ui-conversation")!;
    // Away from the last row first, so landing there is a real move, not a no-op that would pass
    // even if `arrive` touched nothing.
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(container.querySelector(".row-current")!.textContent).toContain("first");
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
     it either. The effect itself is a GUI check; `shell/MANUAL_VERIFICATION.md` owes it. */
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
    it("does not preventDefault Enter on any of the card's own buttons, and claims Space there (v1 S5)", () => {
      vi.useFakeTimers();
      try {
        const { container } = withAToolCallAndAPermission();
        for (const label of ["Approve", "Deny"]) {
          const button = buttonLabelled(container, label)!;
          expect(button, `no button labelled ${label}`).toBeDefined();
          act(() => vi.advanceTimersByTime(300));
          // fireEvent returns false exactly when the default was prevented.
          expect(fireEvent.keyDown(button, { key: "Enter" }), `${label} swallowed Enter`).toBe(true);
          act(() => vi.advanceTimersByTime(300));
          expect(fireEvent.keyDown(button, { key: " " }), `${label} left Space to the button`).toBe(false);
        }
      } finally {
        vi.useRealTimers();
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
    it("leaves Enter on a focused Approve to the button, claims Space (v1 S5), and still navigates from it", () => {
      const { container } = withAToolCallAndAPermission();
      const approve = buttonLabelled(container, "Approve")!;
      // `fireEvent` returns false when the handler called preventDefault, i.e. claimed the key.
      expect(fireEvent.keyDown(approve, { key: "Enter" })).toBe(true);
      expect(fireEvent.keyDown(approve, { key: " " })).toBe(false);
      expect(fireEvent.keyDown(approve, { key: "k" })).toBe(false);
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

  it("Enter on a focused Approve after 300 ms idle approves", () => {
    arrivedOnACard();
    press("l");
    wait(300);
    expect(pressOnButton("Enter")).toBe(true);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
  });

  it("l then Enter at 100 ms still approves: l is the walk onto the button", () => {
    arrivedOnACard();
    press("l");
    wait(100);
    expect(pressOnButton("Enter")).toBe(true);
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
    expect(pressOnButton("Enter")).toBe(true);
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
  function conversation() {
    const rendered = started({ capabilities: { ...initialState().capabilities, interrupt: true } });
    events(
      { type: "user_prompt_submitted", text: "list it" },
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "Run this:\n\n```\nls -la\n```\n\nthen look." },
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
      row.style.lineHeight = "20px";
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
    // "Leader and tab keys" (panel round 2 plan, Task 8) sits between BROWSE and "Typing"; "Slash
    // commands" (spec §9.2, P10) sits between "Typing" and "Anywhere".
    expect(titles).toEqual([
      "This panel",
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
    ]) {
      fireEvent.keyDown(root, key);
    }
    expect(seen).toEqual(["down", "up", "down", "up", "down", "up"]);
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
      lines: over.lines ?? ["切到 bypass 并批准 2 张等待中的卡片？(y/n)"],
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
      dispatchBypassConfirm({ tab: 1, scope: "tab", nonce: 7, lines: ["切到 bypass 并批准 2 张等待中的卡片？(y/n)"] });
      expect(container.querySelector(".band-prompt")!.textContent).toBe("切到 bypass 并批准 2 张等待中的卡片？(y/n)");
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
      dispatchBypassConfirm({ tab: null, scope: "default", nonce: 11, lines: ["新会话默认用 bypass？(y/n)"] });
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
      dispatchBypassConfirm({ nonce: 1, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
      dispatchBypassConfirm({ nonce: 2, lines: ["切到 bypass 并批准 2 张等待中的卡片？(y/n)"] });
      expect(container.querySelector(".band-prompt")!.textContent).toBe("切到 bypass 并批准 2 张等待中的卡片？(y/n)");
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
      dispatchBypassConfirm({ lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 3, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
    wait(300);
    press("y");
    expect(confirms().map((m) => m.nonce)).toEqual([1]);
    wait(20);
    // D7: a card arrived while the prompt was up, so Rust answers with a fresh prompt.
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 2, lines: ["切到 bypass 并批准 2 张等待中的卡片？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 1, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
    wait(100);
    press("y"); // cancels
    press("y", { repeat: true }); // swallowed
    fireEvent.keyUp(document.activeElement ?? document.body, { key: "y" });
    wait(500);
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 2, lines: ["切到 bypass 并批准 1 张等待中的卡片？(y/n)"] });
    wait(300);
    press("y"); // a genuinely fresh, non-repeat press
    expect(confirms().map((m) => m.nonce)).toEqual([2]);
    expect(container.querySelector(".band-prompt")).toBeNull();
  });

  it("the chooser's y over a live tab posts confirm_bypass exactly once", () => {
    fakeClock();
    const { container } = arrivedOnACard();
    dispatch({ kind: "chooser", open: [], records: [] });
    dispatch({ kind: "confirm_bypass", tab: null, scope: "default", nonce: 5, lines: ["新会话默认用 bypass？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: null, scope: "default", nonce: 6, lines: ["新会话默认用 bypass？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["切到 bypass？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 9, lines: ["切到 bypass？(y/n)"] });
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
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 10, lines: ["切到 bypass？(y/n)"] });
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
