// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App, { HINT_PENDING_TIMEOUT_MS, statusWarning } from "./App";
import { initialState } from "./reducer";
import { RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT } from "./follow";
import type { AgentDomainEvent, AgentUiState, Hello, ProviderInfo } from "./types";
import { WHICH_KEY_DELAY_MS } from "./leader";
import { EMPTY_PANEL_TABLE } from "./keymap";
import type { PanelTable } from "./keymap";
import { binding, TABLE } from "./testFixtures";
import { MODE_STARTING_MESSAGE, modeFixedMessage } from "./modeKey";

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
    expect(modeBlock(container).textContent).toBe("BROWSE");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).dataset.focused).toBe("false");
    // The mode itself is untouched: focus is a separate fact from which mode the panel is in.
    expect(modeBlock(container).textContent).toBe("BROWSE");
  });

  it("opens the composer with the caret in it when shell says the user arrived by keyboard", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    expect(modeBlock(container).textContent).toBe("INPUT");
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
    expect(modeBlock(container).textContent).toBe("BROWSE");
  });

  it("does not change the mode or what i does", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    enterInputMode(container);
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).textContent).toBe("INPUT");
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
    expect(modeBlock(container).textContent).toBe("INPUT");
    const seen: string[] = [];
    const onResume = () => seen.push("resume");
    document.addEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    try {
      dispatch({ kind: "arrive" });
    } finally {
      document.removeEventListener(RESUME_FOLLOW_EVENT, onResume, true);
    }
    expect(modeBlock(container).textContent).toBe("BROWSE");
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
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "arrive" });
    expect(modeBlock(container).textContent).toBe("BROWSE");
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
    expect(modeBlock(container).textContent).toBe("INPUT");
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
    expect(modeBlock(container).textContent).toBe("BROWSE");
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
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    expect(modeBlock(container).textContent).toBe("BROWSE");
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
    expect(modeBlock(container).textContent).toBe("BROWSE");
    const current = container.querySelector(".row-current")!;
    expect(current.classList.contains("row-permission")).toBe(true);
    expect(current.textContent).toContain("Permission requested: Bash");
  });

  it("leaves INPUT for the card, so a and d answer it at once", () => {
    const { container } = withTwoCards();
    enterInputMode(container);
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(modeBlock(container).textContent).toBe("BROWSE");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
  });

  it("takes the composer when the card was answered in between", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "focus_permission", tab: 1 });
    expect(modeBlock(container).textContent).toBe("INPUT");
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
    expect(posted).toHaveLength(1);
    expect(posted[0].type).toBe("ready");
    expect(typeof posted[0].request_id).toBe("string");
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
    expect(container.querySelector(".status-band .mode-pill")?.textContent).toBe("⏵⏵ bypass");
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
    expect(container.querySelector(".composer-browse-hint")!.textContent).toContain(
      "Press r to start a new session here.",
    );
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
    expect(container.querySelector(".tool-result-folded")).not.toBeNull();

    fireEvent.keyDown(conversationRoot(container), { key: "Enter" });
    expect(container.querySelector(".tool-result-folded")).toBeNull();

    fireEvent.keyDown(conversationRoot(container), { key: "Enter" });
    expect(container.querySelector(".tool-result-folded")).not.toBeNull();
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
    it("does not preventDefault Enter or Space on any of the card's own buttons", () => {
      const { container } = withAToolCallAndAPermission();
      for (const label of ["Approve", "Deny"]) {
        const button = buttonLabelled(container, label)!;
        expect(button, `no button labelled ${label}`).toBeDefined();
        for (const key of ["Enter", " "]) {
          // fireEvent returns false exactly when the default was prevented.
          expect(fireEvent.keyDown(button, { key }), `${label} swallowed ${JSON.stringify(key)}`).toBe(true);
        }
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
    it("leaves Enter and Space on a focused button to the button, and still navigates from it", () => {
      const { container } = withAToolCallAndAPermission();
      const approve = buttonLabelled(container, "Approve")!;
      // `fireEvent` returns false when the handler called preventDefault, i.e. claimed the key.
      expect(fireEvent.keyDown(approve, { key: "Enter" })).toBe(true);
      expect(fireEvent.keyDown(approve, { key: " " })).toBe(true);
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
      expect(hint.textContent).toContain("Press r to start a new session here.");
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

  it("H posts tab_verb prev, [ b posts prev via the table, [ [ is still a prompt jump", () => {
    const { container } = started();
    sendTable();
    act(() => root(container).focus());
    press("H");
    expect(tabVerbsPosted()).toEqual(["prev"]);
    posted.length = 0;
    press("[");
    press("b");
    expect(tabVerbsPosted()).toEqual(["prev"]);
    posted.length = 0;
    press("[");
    press("[");
    expect(posted.length).toBe(0);
  });

  it("Space with the Approve button focused activates the button, starting no sequence", () => {
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
      // `fireEvent` returns false only when the handler called `preventDefault` -- true means the
      // key was left to the button's own native activation (the same convention the Approve-
      // unreachable regression's own tests use).
      expect(fireEvent.keyDown(approve, { key: " " })).toBe(true);
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

  it("<leader>m flashes on a live session", () => {
    const widen = stubBandWidth();
    const { container } = started();
    act(() => widen(container));
    sendTable();
    act(() => root(container).focus());
    press(" ");
    press("m");
    // Wave 4 Task 1: the message now names how to get a different mode (`modeFixedMessage`);
    // `sendTable()` dispatched `newTabChord: "Ctrl+b c"` above.
    expect(container.querySelector(".band-message")!.textContent).toBe(
      "mode is fixed for this session — Ctrl+b c for a new tab",
    );
    expect(posted.some((p) => p.type === "cycle_mode")).toBe(false);
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
    const { container } = started();
    sendTable(withExtras);
    act(() => root(container).focus());

    press(" ");
    press("/");
    expect(container.querySelector(".search-bar")).not.toBeNull();
    act(() => root(container).focus());

    press(" ");
    press("?");
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
    // Close it so it does not swallow the next press.
    fireEvent.keyDown(container.querySelector(".keymap-overlay")!, { key: "Escape" });
    act(() => root(container).focus());

    posted.length = 0;
    press(" ");
    press("t");
    expect(posted.length).toBe(0);
    expect(container.querySelector(".handoff")).not.toBeNull();
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

  it("a on the tool call answers the card that gates it, without moving onto the card", () => {
    withACardAndStop();
    press("a");
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
  });

  it("d on the card itself denies it", () => {
    withACardAndStop();
    press("j");
    press("d");
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "deny" });
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
    withACardAndStop();
    press("j");
    press("j");
    press("a");
    expect(lastOfType("permission_response")).toBeUndefined();
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
   *  `data-nav-order` (Approve, Deny, reason), then the activity line's Stop, then the always-present
   *  band (panel round 2 plan, Task 10; its one control at width 0 -- `band.ts`'s own
   *  pre-measurement floor -- is `.band-open`, `data-nav-stop="status-band"`'s own only child that
   *  matches `controlsOf`'s selector). */
  const AT = { prompt: 0, reply: 1, code: 2, tool: 3, card: 4, approve: 5, deny: 6, reason: 7, stop: 8, band: 9 };

  it("f in BROWSE asks shell for a HINT, and f in INPUT is just a letter", () => {
    const { container } = conversation();
    press("f");
    expect(lastOfType("hint_request")).toMatchObject({ type: "hint_request" });
    expect(typeof lastOfType("hint_request")!.request_id).toBe("string");
    // shell answers and the HINT ends; only then are the panel's keys its own again.
    collect(1);
    dispatch({ kind: "hint_end", sessionId: 1 });
    posted = [];
    fireEvent.keyDown(root(container), { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "f" });
    expect(lastOfType("hint_request")).toBeUndefined();
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
      const { container } = onTheCard();
      press("f");
      press("i");
      press("j");
      press("f");
      expect(container.querySelector("textarea")).toBeNull();
      expect(container.querySelector(".row-current")).toBe(container.querySelector(".row-permission"));
      expect(posted.filter((m) => m.type === "hint_request")).toHaveLength(1);
    });

    it("once the HINT it asked for has ended, the keys are the panel's again", () => {
      const { container } = onTheCard();
      press("f");
      collect(2);
      dispatch({ kind: "hint_end", sessionId: 2 });
      press("a");
      expect(lastOfType("permission_response")).toBeDefined();
      expect(container.querySelector(".row-permission")).not.toBeNull();
    });

    it("a shell that never answers costs a second of dead keys, not a stuck panel", () => {
      vi.useFakeTimers();
      try {
        const { container } = onTheCard();
        press("f");
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
    // The prompt row, then the composer, then the always-present band (panel round 2 plan, Task 10).
    expect(collect(1)).toBe(3);
    show(1, 3);
    expect(labels(container)[1].classList.contains("hint-composer")).toBe(true);
    dispatch({ kind: "hint_land", sessionId: 1, index: 1 });
    const textarea = container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    expect(document.activeElement).toBe(textarea);
    expect(lastOfType("send_message")).toBeUndefined();
  });

  it("offers no composer label only once the session has ended (C1: a running turn no longer disables it)", () => {
    // A running turn no longer disables the composer (C1: it queues a follow-up instead), so it is
    // a HINT target through the turn too -- one more than `AT`'s ten.
    const running = conversation();
    box(running.container.querySelector(".composer")!, 900);
    expect(collect(1)).toBe(Object.keys(AT).length + 1);
    cleanup();
    const { container } = idle();
    expect(collect(2)).toBe(3);
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
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    const textarea = container.querySelector("textarea")!;
    fireEvent.keyDown(textarea, { key: "f" });
    expect(lastOfType("hint_request")).toBeUndefined();
    fireEvent.blur(textarea);
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "f" });
    expect(lastOfType("hint_request")).toBeDefined();
  });

  it("hint_collect reports how many targets are on screen, and leaves off-screen ones out", () => {
    const { container } = conversation();
    // 4 rows (prompt, reply, tool call, card) + the reply's code block + Approve + Deny + the
    // reason box + Stop + the always-present band (panel round 2 plan, Task 10).
    const count = collect(1);
    expect(count).toBe(Object.keys(AT).length);
    // Push the card row (and everything in it) below the list's viewport: it is no longer counted.
    const card = container.querySelector(".row-permission")!;
    box(card, 2000);
    for (const el of card.querySelectorAll("button, input")) box(el, 2000);
    expect(collect(2)).toBe(6);
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

  it("opens on ? and shows all five groups, in the spec's order", () => {
    const { container } = started();
    act(() => root(container).focus());
    expect(overlay(container)).toBeNull();
    // `?` almost always arrives as Shift+/ -- the same reason `resolveKey`'s own test checks it
    // both ways (`keymap.test.ts`).
    press("?", { shiftKey: true });
    const el = overlay(container);
    expect(el).not.toBeNull();
    const titles = Array.from(el!.querySelectorAll("h2")).map((h) => h.textContent);
    // "Leader and tab keys" (panel round 2 plan, Task 8) sits between BROWSE and "Anywhere".
    expect(titles).toEqual(["This panel", "Leader and tab keys", "Typing", "Anywhere in the window", "After Ctrl+b"]);
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
    expect(container.querySelector<HTMLElement>("[data-testid=mode-block]")!.textContent).toBe("BROWSE");
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
    // `m` would cycle the mode on the dashboard; under the overlay it is swallowed.
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "m" });
    expect(lastOfType("cycle_mode")).toBeUndefined();
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
   *  not even `i` recovered it; only a click did. The empty tab's live control is its composer.
   *  Wave 4 R2: D10's launch chooser is gone, so `Esc` here always returns the keys this way,
   *  entirely locally -- there is no round trip to Rust for it any more. */
  it("Esc from prefix w over an empty tab gives the composer the keys, in INPUT", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchEmptyTab();
    dispatch(ENV);
    const postsBefore = posted.length;
    fireEvent.keyDown(container.querySelector(".chooser")!, { key: "Escape" });
    expect(posted.length).toBe(postsBefore);
    expect(container.querySelector(".chooser")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
  });
});

describe("phase 3 lines, and the band's message/prompt (panel round 2 plan, Task 10)", () => {
  it("flashes that the mode is fixed on Shift+Tab after the start, and never offers a cycle (D6)", () => {
    vi.useFakeTimers();
    const widen = stubBandWidth();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState(), 1);
      act(() => widen(container));
      expect(container.querySelector(".status-band .mode-pill")!.textContent).toBe("⏵⏵ auto");
      enterInputMode(container);
      fireEvent.keyDown(container.querySelector("textarea")!, { key: "Tab", shiftKey: true });
      // Wave 4 Task 1: the message now names how to get a different mode (`modeFixedMessage`); no
      // `keymap` envelope was dispatched here, so `newTabChord` is still the pre-envelope default.
      expect(container.querySelector(".band-message")!.textContent).toBe(
        "mode is fixed for this session — open a new tab to choose",
      );
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
    expect(container.querySelector(".status-band .mode-pill")!.textContent).toBe("⏵⏵ bypass");
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
    expect(container.querySelector("[data-testid=mode-block]")!.textContent).toBe("BROWSE");
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
  });

  it("a answers the only card from any row", () => {
    const { container } = oneCard();
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "a" });
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
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
    expect(container.querySelector("[data-testid=mode-block]")!.textContent).toBe("INPUT");
    expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — Esc, then a / d");
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
    expect(container.querySelector("[data-testid=mode-block]")!.textContent).toBe("INPUT");
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
    expect(container.querySelectorAll(".tool-result-folded")).toHaveLength(3);
    fireEvent.keyDown(root, { key: "o", ctrlKey: true });
    expect(container.querySelectorAll(".tool-result-folded")).toHaveLength(0);
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "tabs", active: 1, tabs: two });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state });
    expect(container.querySelectorAll(".row-tool")).toHaveLength(3);
    expect(container.querySelectorAll(".tool-result-folded"), "the detailed view is the tab's own").toHaveLength(0);
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

  it("D puts the keys in the only card's reason box; Enter there denies with the reason", () => {
    const { container } = withBash();
    const root = container.querySelector(".agent-ui-conversation")!;
    fireEvent.keyDown(root, { key: "D", shiftKey: true });
    const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
    expect(document.activeElement).toBe(reason);
    fireEvent.change(reason, { target: { value: "keep the build" } });
    fireEvent.keyDown(reason, { key: "Enter" });
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "p1", decision: "deny", reason: "keep the build" });
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
    expect(modeBlock(container).textContent).toBe("BROWSE");
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

  it("a. live tab, BROWSE, focus on the conversation root: no cycle_mode, the band names the way out", () => {
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
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage(""));
    vi.unstubAllGlobals();
  });

  it("b. live tab, INPUT, focus in the composer textarea: same flash, textarea keeps focus and its text", () => {
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
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage(""));
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

  it("g. a starting tab: flash, no post", () => {
    // Wave 5 (W5): a starting tab always says so -- the fixed-mode text would be false on a
    // switch-capable sidecar, so `modeKeyRoute` never shows it here regardless of `canSwitch`.
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
    act(() => widen(container));
    const root = container.querySelector(".empty-tab")!;
    const event = shiftTab(root);
    expect(event.defaultPrevented).toBe(true);
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(container.querySelector(".band-message")!.textContent).toBe(MODE_STARTING_MESSAGE);
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

  it("i. <leader> mode.cycle on a live tab flashes the same modeFixedMessage text", () => {
    const widen = stubBandWidth();
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatchLiveTab(snapshotState(), 1);
    act(() => widen(container));
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    const root = container.querySelector(".agent-ui-conversation")!;
    act(() => (root as HTMLElement).focus());
    fireEvent.keyDown(root, { key: " " });
    fireEvent.keyDown(root, { key: "m" });
    expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage("Ctrl+b c"));
    expect(lastOfType("cycle_mode")).toBeUndefined();
    vi.unstubAllGlobals();
  });
  /** Whole-branch review: R4 names Shift+Tab and `<leader>` `mode.cycle` as one rule, but on a
   *  `starting` or `failed` tab `EmptyTab` returned before its leader engine, so `<leader>m` did
   *  nothing at all; and `runPanelAction` gated on `live || ended`, so reaching it there would have
   *  posted a `cycle_mode` Rust only refuses. Both now follow `modeKeyRoute`. */
  it("j. <leader> mode.cycle on a failed tab flashes like Shift+Tab, no post", () => {
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
    expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage("Ctrl+b c"));
    vi.unstubAllGlobals();
  });

  // Wave 5 (W5): a starting tab flashes the "session is starting" text instead, both for Shift+Tab
  // (test g) and here for `<leader> mode.cycle` -- `runPanelAction`'s own `modeKeyRoute` switch.
  it("j2. <leader> mode.cycle on a starting tab flashes the starting text, no post", () => {
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
    expect(lastOfType("cycle_mode")).toBeUndefined();
    expect(container.querySelector(".band-message")!.textContent).toBe(MODE_STARTING_MESSAGE);
    vi.unstubAllGlobals();
  });

  describe("Shift+Tab switches a live tab (wave 5)", () => {
    const switchCapable = { ...initialState().capabilities, modeSwitch: true };

    it("a. live tab, switch-capable sidecar, BROWSE: one cycle_mode, no flash, focus unchanged", () => {
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ capabilities: switchCapable }), 1);
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

    it("b. same in INPUT, text in the composer: posted, text and focus kept", () => {
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ capabilities: switchCapable }), 1);
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

    it("c. without the capability: wave 4's fixed flash, nothing posted", () => {
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ capabilities: { ...initialState().capabilities, modeSwitch: false } }), 1);
      act(() => widen(container));
      const root = container.querySelector(".agent-ui-conversation")!;
      act(() => (root as HTMLElement).focus());
      const event = shiftTab(root);
      expect(event.defaultPrevented).toBe(true);
      expect(lastOfType("cycle_mode")).toBeUndefined();
      expect(container.querySelector(".band-message")!.textContent).toBe(modeFixedMessage(""));
      vi.unstubAllGlobals();
    });

    it("d. a starting tab: MODE_STARTING_MESSAGE, nothing posted -- never the fixed-mode text", () => {
      // `modeKeyRoute`'s order puts `starting` ahead of `fixed` unconditionally (W5): a starting
      // tab has no session yet, so `state.capabilities` (reset on every tab switch, along with the
      // rest of the per-session projection) cannot yet say whether this one will switch -- but
      // showing the fixed-mode text here would still be a lie on a switch-capable sidecar, so
      // `modeKeyRoute` never shows it regardless.
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "tabs", active: 1, tabs: [{ ...LIVE_TAB, state: "starting" }] });
      act(() => widen(container));
      const root = container.querySelector(".empty-tab")!;
      const event = shiftTab(root);
      expect(event.defaultPrevented).toBe(true);
      expect(lastOfType("cycle_mode")).toBeUndefined();
      expect(container.querySelector(".band-message")!.textContent).toBe(MODE_STARTING_MESSAGE);
      vi.unstubAllGlobals();
    });

    it("e. <leader> mode.cycle on a live switch-capable tab: posted, box does not grey it out", () => {
      const widen = stubBandWidth();
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatchLiveTab(snapshotState({ capabilities: switchCapable }), 1);
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
  });
});
