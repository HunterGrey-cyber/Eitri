// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App, { HINT_PENDING_TIMEOUT_MS, WHICH_KEY_G_PREFIX_DELAY_MS } from "./App";
import { initialState } from "./reducer";
import { USER_SCROLL_EVENT } from "./follow";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
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
};

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
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

function buttonLabelled(container: HTMLElement, label: string): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(label));
}

describe("pane focus", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }

  it("draws the mode block dim until shell says this pane has focus, and follows it both ways", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
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
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    expect(modeBlock(container).textContent).toBe("INPUT");
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    expect(container.textContent).not.toContain("按 i 开始输入");
  });

  it("does not open the composer on a session that has ended, the same as i", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    dispatch({
      kind: "events",
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
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "pane_focus", focused: true });
    enterInputMode(container);
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "pane_focus", focused: false });
    expect(modeBlock(container).textContent).toBe("INPUT");
    expect(modeBlock(container).dataset.focused).toBe("false");
  });
});

describe("focus_permission (the tray's agent chip, or Ctrl+a a, with a card waiting)", () => {
  function modeBlock(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>("[data-testid=mode-block]")!;
  }
  function withTwoCards() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
    const list: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "tidy up" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { cmd: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_2", name: "Write", input: { file_path: "a" } },
      { type: "permission_requested", permission_id: "perm-2", tool_use_id: "toolu_2", tool_name: "Write", input: {} },
    ];
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
    dispatch({ kind: "pane_focus", focused: true });
    return rendered;
  }

  it("lands in BROWSE on the oldest pending card", () => {
    const { container } = withTwoCards();
    dispatch({ kind: "focus_permission" });
    expect(modeBlock(container).textContent).toBe("BROWSE");
    const current = container.querySelector(".row-current")!;
    expect(current.classList.contains("row-permission")).toBe(true);
    expect(current.textContent).toContain("Permission requested: Bash");
  });

  it("leaves INPUT for the card, so a and d answer it at once", () => {
    const { container } = withTwoCards();
    enterInputMode(container);
    expect(modeBlock(container).textContent).toBe("INPUT");
    dispatch({ kind: "focus_permission" });
    expect(modeBlock(container).textContent).toBe("BROWSE");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    expect(lastOfType("permission_response")).toMatchObject({ permission_id: "perm-1", decision: "allow" });
  });

  it("takes the composer when the card was answered in between", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "focus_permission" });
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
    dispatch({ kind: "focus_permission" });
    expect(seen).toEqual(["unknown"]);
  });
});

describe("select_all (Ctrl+a Ctrl+a from shell's prefix)", () => {
  it("selects all of the composer textarea's text while it has focus", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    dispatch({ kind: "pane_focus", focused: true });
    enterInputMode(container);
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello world" } });
    dispatch({ kind: "select_all" });
    expect(textarea.selectionStart).toBe(0);
    expect(textarea.selectionEnd).toBe(textarea.value.length);
  });

  it("does nothing in BROWSE, where no text field has focus", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState() });
    const before = document.activeElement;
    expect(() => dispatch({ kind: "select_all" })).not.toThrow();
    expect(document.activeElement).toBe(before);
  });
});

describe("App handshake", () => {
  it("announces itself with a `ready` carrying a request id, before anything else", () => {
    render(<App />);
    expect(posted).toHaveLength(1);
    expect(posted[0].type).toBe("ready");
    expect(typeof posted[0].request_id).toBe("string");
  });

  it("waits for hello rather than guessing what the backend offers", () => {
    const { container } = render(<App />);
    expect(container.textContent).toContain("Connecting to the shell");
    dispatch({ kind: "hello", ...HELLO });
    expect(container.textContent).toContain("/home/user/project");
  });

  it("starts a session with the mode that was clicked, and shows that it is connecting", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    fireEvent.click(buttonLabelled(container, "Bypass")!);
    const start = lastOfType("start_session")!;
    expect(start.mode).toBe("bypass");
    // A fresh session sends no resume id at all -- never an empty string, which Rust would parse as
    // a request to continue a session named "".
    expect(start.resume).toBeUndefined();
    expect(container.textContent).toContain("Starting the agent backend");
  });

  it("remembers the permission mode it started with and shows it in the winbar", () => {
    // AgentUiState carries no field for this -- App.tsx remembers what it asked for, because the
    // provider refuses a mode it cannot honour rather than substituting one. See
    // `startedPermissionMode`'s own doc comment in App.tsx.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    fireEvent.click(buttonLabelled(container, "Bypass")!);
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
    expect(container.querySelector(".winbar .permission-mode")?.textContent).toBe("bypass");
  });

  it("returns to the start screen when the deferred start_session reply says it failed", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    fireEvent.click(buttonLabelled(container, "Auto")!);
    const requestId = lastOfType("start_session")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "claude is not on PATH" });
    expect(container.textContent).toContain("/home/user/project");
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
    dispatch({
      kind: "snapshot",
      throughRevision: 7,
      state: snapshotState({ model: "claude-opus-5", transcript: [{ seq: 0, text: "pre-reload marker alpha seven." }] }),
    });
    expect(container.querySelector(".agent-ui-conversation")).not.toBeNull();
    expect(container.querySelector(".mode-selector")).toBeNull();
    expect(container.textContent).toContain("pre-reload marker alpha seven.");
  });

  it("ignores a command_result for a request this document never sent", () => {
    // Exactly what a reload produces: the reply to the pre-reload page's `start_session` arrives at
    // a page that has no record of it. It must not be read as this page's own start failing.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState({ transcript: [{ seq: 0, text: "still here" }] }) });
    dispatch({ kind: "command_result", requestId: "req-from-a-previous-page", ok: true });
    expect(container.textContent).toContain("still here");
    expect(container.querySelector(".mode-selector")).toBeNull();
  });
});

describe("App event folding", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
    enterInputMode(rendered.container);
    return rendered;
  }

  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
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

  it("disables the composer for the life of a real turn, on real events only", () => {
    const { container } = startedApp();
    expect(container.querySelector("textarea")!.disabled).toBe(false);
    events({ type: "turn_started", turn_id: "t1" });
    expect(container.querySelector("textarea")!.disabled).toBe(true);
    events({
      type: "turn_completed",
      turn_id: "t1",
      outcome: "completed",
      result_text: "",
      stop_reason: null,
      usage: { total_cost_usd: 0, num_turns: 1 },
    });
    expect(container.querySelector("textarea")!.disabled).toBe(false);
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
      "Press r to return to the start screen.",
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
   is exactly the distinction `App.tsx` -- not the reducer, and not `StatusLine`/`TurnActivity` --
   is positioned to know, since only the raw envelope carries it. Tested here, at the layer that
   actually decides it. */
describe("the in-flight motion indicator's elapsed clock", () => {
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
  }

  it("is exact when the turn id was learned from a real turn_started event", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
    events({ type: "turn_started", turn_id: "t1" });
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s");
  });

  it("is inexact when the turn id first arrives inside a snapshot -- a reload, or a resync mid-turn", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState({ activeTurnId: "t1" }) });
    expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s+");
  });

  it("does not restart -- keyed on the turn id, so a resync that repeats it leaves `since` untouched", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState({ activeTurnId: "t1" }) });
      act(() => {
        vi.advanceTimersByTime(5000);
      });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("5s+");
      // The same turn id again, as a resync mid-turn resends it: the clock keeps counting from the
      // original `since` rather than starting over from "now".
      dispatch({ kind: "snapshot", throughRevision: 2, state: snapshotState({ activeTurnId: "t1" }) });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("5s+");
    } finally {
      vi.useRealTimers();
    }
  });

  it("is cleared by a terminal event, and a later turn starts its own clock from zero", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
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
        dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
        events({ type: "turn_started", turn_id: "t1" });
        act(() => {
          vi.advanceTimersByTime(60_000);
        });
        expect(container.querySelector(".turn-elapsed")?.textContent).toBe("60s");

        if (reset === "a fatal error") {
          dispatch({ kind: "error", message: "the session died" });
        } else {
          dispatch({
            kind: "handoff",
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
        dispatch({ kind: "snapshot", throughRevision: 9, state: snapshotState({ activeTurnId: "t1" }) });
        expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s+");
      } finally {
        vi.useRealTimers();
      }
    });
  }

  it("a panel reload loses the clock, so it re-reads 0s+ and counts up from there -- less information, never a false statement", () => {
    // A "reload" here is simply a fresh App mount receiving its first snapshot with a turn already
    // active -- `turnClock` starts at `null` on every mount, same as `state` starts at
    // `initialState()`, and there is no persisted copy of it to restore.
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 5, state: snapshotState({ activeTurnId: "already-running" }) });
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
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
    return rendered;
  }

  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
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
    expect(container.querySelector(".mode-selector")).toBeNull();
  });

  it("r returns to the start screen once the session has ended, and asks for a fresh hello", () => {
    const { container } = startedApp();
    events({ type: "session_closed", reason: "provider exited" });
    const readiesBefore = posted.filter((m) => m.type === "ready").length;

    fireEvent.keyDown(conversationRoot(container), { key: "r" });

    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    expect(container.querySelector(".mode-selector")).not.toBeNull();
    expect(posted.filter((m) => m.type === "ready").length).toBe(readiesBefore + 1);
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
      expect(hint.textContent).toContain("Press r to return to the start screen.");
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
      expect(container.querySelector(".mode-selector")).not.toBeNull();
    });
  });
});

describe("App: the which-key strip (spec 2026-09-19-which-key-design.md)", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState(overrides) });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key });
  const strip = (c: HTMLElement) => c.querySelector<HTMLElement>(".which-key");
  const keycaps = (el: HTMLElement) => Array.from(el.querySelectorAll(".keycap")).map((k) => k.textContent);

  it("is present in BROWSE, and gone once i opens INPUT", () => {
    const { container } = started();
    act(() => root(container).focus());
    expect(strip(container)).not.toBeNull();
    press("i");
    expect(strip(container)).toBeNull();
  });

  it("lists only ? keys for an ordinary row", () => {
    const { container } = started();
    events({ type: "user_prompt_submitted", text: "hi" });
    act(() => root(container).focus());
    expect(keycaps(strip(container)!)).toEqual(["?"]);
  });

  it("lists a allow / d deny / l buttons on the pending permission under the cursor", () => {
    const { container } = started();
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: {} },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: {} },
    );
    act(() => root(container).focus());
    // The cursor opens on the tool row (index 0), which the card right after it gates -- no `j`
    // needed, and this is what proves it works from the GATING row, not only from the card itself.
    const text = strip(container)!.textContent!;
    expect(text).toContain("allow");
    expect(text).toContain("deny");
    expect(text).toContain("buttons");
  });

  it("lists r new session once the session has ended", () => {
    const { container } = started();
    events({ type: "session_closed", reason: "provider exited" });
    act(() => root(container).focus());
    const text = strip(container)!.textContent!;
    expect(text).toContain("new session");
  });

  describe("the g prefix line (spec §2.3, 400ms)", () => {
    it("shows nothing before the delay, `first row` once it elapses, and clears on the next key", () => {
      vi.useFakeTimers();
      try {
        const { container } = started();
        events({ type: "user_prompt_submitted", text: "hi" });
        act(() => root(container).focus());
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_G_PREFIX_DELAY_MS - 1));
        expect(strip(container)!.textContent).not.toContain("first row");
        act(() => vi.advanceTimersByTime(1));
        expect(strip(container)!.textContent).toContain("first row");
        press("j");
        expect(strip(container)!.textContent).not.toContain("first row");
      } finally {
        vi.useRealTimers();
      }
    });

    it("gg inside the delay never shows the prefix line", () => {
      vi.useFakeTimers();
      try {
        const { container } = started();
        events({ type: "user_prompt_submitted", text: "hi" });
        act(() => root(container).focus());
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_G_PREFIX_DELAY_MS - 1));
        press("g");
        act(() => vi.advanceTimersByTime(WHICH_KEY_G_PREFIX_DELAY_MS));
        expect(strip(container)!.textContent).not.toContain("first row");
      } finally {
        vi.useRealTimers();
      }
    });
  });
});

describe("App keyboard: every control is reachable with hjkl", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState(overrides) });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
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
    expect(document.activeElement?.textContent).toBe("Stop");
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

  it("walks the start screen's buttons with j and k", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    const modes = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="mode"]'));
    expect(modes).toHaveLength(2);
    press("j");
    expect(document.activeElement).toBe(modes[0]);
    press("j");
    expect(document.activeElement).toBe(modes[1]);
    press("j");
    expect(document.activeElement).toBe(modes[1]);
    press("k");
    expect(document.activeElement).toBe(modes[0]);
  });
});

/* The panel's half of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
   `shell` owns the session and every key typed during it; the panel answers five envelopes. jsdom lays
   nothing out, so `layOut` gives every stop, control and code block a 10px box inside a 1000px list,
   one below the other, and the list and root the whole height. Nothing here shows what WebKit draws. */
describe("App global HINT: the panel's half", () => {
  function started(overrides: Partial<AgentUiState> = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState(overrides) });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
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
   *  `data-nav-order` (Approve, Deny, reason), then the status line's Stop. */
  const AT = { prompt: 0, reply: 1, code: 2, tool: 3, card: 4, approve: 5, deny: 6, reason: 7, stop: 8 };

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
    // The prompt row, then the composer.
    expect(collect(1)).toBe(2);
    show(1, 2);
    expect(labels(container)[1].classList.contains("hint-composer")).toBe(true);
    dispatch({ kind: "hint_land", sessionId: 1, index: 1 });
    const textarea = container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    expect(document.activeElement).toBe(textarea);
    expect(lastOfType("send_message")).toBeUndefined();
  });

  it("offers no composer label while it cannot take a message: a turn running, or the session ended", () => {
    // `conversation()` has a turn in progress: the box is disabled, and a landing could not focus it.
    const running = conversation();
    box(running.container.querySelector(".composer")!, 900);
    expect(collect(1)).toBe(Object.keys(AT).length);
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

  it("f on the start screen asks shell for a HINT too", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    press("f");
    expect(lastOfType("hint_request")).toBeDefined();
  });

  it("hint_collect reports how many targets are on screen, and leaves off-screen ones out", () => {
    const { container } = conversation();
    // 4 rows (prompt, reply, tool call, card) + the reply's code block + Approve + Deny + the
    // reason box + Stop.
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
    const stop = buttonLabelled(container, "Stop")!;
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

  it("labels the start screen's buttons, where there is no conversation yet", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    layOut(container);
    const count = collect(1);
    expect(count).toBe(container.querySelectorAll('[data-nav-stop="mode"]').length + container.querySelectorAll('[data-nav-stop="choice"]').length);
    show(1, count);
    expect(labels(container)).toHaveLength(count);
    const mode = container.querySelector<HTMLElement>('[data-nav-stop="mode"]')!;
    const index = Array.from(container.querySelectorAll<HTMLElement>("[data-nav-stop]")).indexOf(mode);
    dispatch({ kind: "hint_land", sessionId: 1, index });
    expect(document.activeElement).toBe(mode);
    expect(lastOfType("start_session")).toBeUndefined();
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
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState(overrides) });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
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
    const stop = buttonLabelled(container, "Stop")!;
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
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState(overrides) });
    return rendered;
  }

  it("offers the control but disabled, with the reason visible, before the first turn", () => {
    const { container } = conversation({ providerSessionId: null });
    expect(buttonLabelled(container, "Continue in a terminal")!.disabled).toBe(true);
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("first turn");
  });

  it("asks Rust for the handoff only after the confirmation, never on the first click", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    fireEvent.click(buttonLabelled(container, "Continue in a terminal")!);
    expect(lastOfType("handoff_to_terminal")).toBeUndefined();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    const posted = lastOfType("handoff_to_terminal")!;
    expect(typeof posted.request_id).toBe("string");
    // Nothing about WHICH session: Rust reads that from canonical state, and a second source for it
    // is how a panel eventually prints a command resuming some other conversation.
    expect(Object.keys(posted).sort()).toEqual(["request_id", "type"]);
  });

  /* The envelope arrives only after the real close has finished, so by the time this renders the
     conversation genuinely is over -- which is why the transcript goes with it rather than being
     left on screen looking live. */
  it("replaces the conversation with the command once the session really has been closed", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2", transcript: [{ seq: 1, text: "earlier reply" }] });
    dispatch({
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    expect(container.querySelector("pre.handoff-command")!.textContent).toBe(
      "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
    );
    // The sign column this row adopted: nothing previously asserted it carries `→`.
    expect(container.querySelector(".row-handoff")!.getAttribute("data-sign")).toBe("→");
    expect(container.querySelector(".agent-ui-conversation")).toBeNull();
    expect(container.textContent).not.toContain("earlier reply");
    // A new conversation is still startable; only this one moved.
    expect(container.querySelector(".mode-selector")).not.toBeNull();
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
      kind: "handoff",
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
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    fireEvent.click(buttonLabelled(container, "Auto")!);
    const requestId = lastOfType("start_session")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "claude is not on PATH" });
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
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    const clipboard = stubClipboard();
    fireEvent.keyDown(container.querySelector(".agent-ui-root")!, { key: "y" });
    expect(clipboard.writeText).toHaveBeenCalledWith("cd /home/user/project && claude --resume 1857dcd5-973b-46a2");
  });

  /* ...and a session that really does start replaces it. The card is only ever drawn on the start
     screen, so the way this becomes visible is the round trip: hand off, start a session that runs,
     then have THAT session die — the start screen must show the new session's error, not the old
     conversation's command. */
  it("clears the command once a real session is running, so a later failure does not resurrect it", () => {
    const { container } = conversation({ providerSessionId: "1857dcd5-973b-46a2" });
    dispatch({
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    expect(container.querySelector("pre.handoff-command")).not.toBeNull();

    dispatch({ kind: "snapshot", throughRevision: 9, state: snapshotState() });
    dispatch({ kind: "error", message: "the second session died" });
    expect(container.querySelector(".mode-selector")).not.toBeNull();
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
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    });
    // The picker renders one radio per remembered session (plus "New session"), so the assertion
    // is that the unrelated row survived -- not that some singular "continue" control exists. That
    // control is gone: the offer became a list the same day this suppression was written.
    const offered = Array.from(container.querySelectorAll(".session-choice button.resume")).map(
      (b) => b.textContent ?? "",
    );
    // The picker shows `shortId` -- the first eight characters -- so the expected substrings are
    // the prefixes as rendered, not the full ids.
    expect(offered.some((label) => label.includes("some-oth"))).toBe(true);
    expect(offered.some((label) => label.includes("1857dcd5"))).toBe(false);
  });
});

/* The close window is not instantaneous — on the sidecar path `AgentBackend::shutdown` is a 10s
   unary RPC plus kill escalation, on legacy ~0.8s of grace periods — and for its whole length Rust
   holds no session and refuses every command. The frontend has to reflect that. */
describe("App while a handoff is closing the conversation", () => {
  function closing() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({
      kind: "snapshot",
      throughRevision: 1,
      state: snapshotState({ providerSessionId: "1857dcd5-973b-46a2" }),
    });
    enterInputMode(rendered.container);
    fireEvent.click(buttonLabelled(rendered.container, "Continue in a terminal")!);
    return rendered;
  }

  /* The outcome that is not acceptable is the message disappearing with no trace. It is still in
     the box, nothing was sent, and the box says why it stopped accepting input. */
  it("does not swallow a message typed before the conversation started closing", () => {
    const { container } = closing();
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "a long prompt worth not losing" } });
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);

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
    expect(buttonLabelled(container, "Continue in a terminal")!.disabled).toBe(true);
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("being closed");
  });

  /* A stale-view refusal (a turn started between the render and the click) leaves the session
     completely untouched on the Rust side, so the panel has to come back to life here too. */
  it("comes back to life, saying why, when Rust refuses the handoff", () => {
    const { container } = closing();
    fireEvent.click(buttonLabelled(container, "Close it and show me the command")!);
    const requestId = lastOfType("handoff_to_terminal")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "A turn is still running." });
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
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState() });
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

  it("does not double-report a failed start, which already has its own banner", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    fireEvent.click(buttonLabelled(container, "Auto")!);
    const requestId = lastOfType("start_session")!.request_id;
    dispatch({ kind: "command_result", requestId, ok: false, error: "claude is not on PATH" });
    expect(container.querySelector(".command-notice")).toBeNull();
  });
});

describe("App fatal errors", () => {
  it("shows the whole error text and returns to the start screen", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState({ transcript: [{ seq: 0, text: "gone" }] }) });
    dispatch({ kind: "error", message: "sidecar handshake failed\nclaude CLI 2.1.272 is untested" });
    const banner = container.querySelector(".fatal-error")!;
    // <pre>, because the sidecar's diagnostics are multi-line and the exact text is the point.
    expect(banner.querySelector("pre")!.textContent).toContain("claude CLI 2.1.272 is untested");
    expect(container.querySelector(".mode-selector")).not.toBeNull();
    // The dead session's transcript is gone with it, rather than left on screen looking live.
    expect(container.textContent).not.toContain("gone");
  });

  /* The start screen's session picker is built from `hello`, which arrives once on mount. A session
     that dies is persisted BEFORE it dies (`conversation::persist_record` on adoption), so by the
     time the user is looking at the picker again that session is on disk and offerable -- but the
     component is still rendering the list it captured at mount, which does not contain it. Asking
     for `hello` again is what makes the picker's own "Previous conversations here, newest first"
     true at the moment it is shown. */
  it("asks for a fresh hello when a fatal error drops it back to the start screen", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    expect(posted.filter((m) => m.type === "ready")).toHaveLength(1);

    dispatch({ kind: "error", message: "the provider exited" });

    const readies = posted.filter((m) => m.type === "ready");
    expect(readies).toHaveLength(2);
    // A distinct request id, not the mount one replayed: Rust answers each `ready` with its own
    // `command_result`, and two replies to one id is a bookkeeping bug waiting to happen.
    expect(readies[1].request_id).not.toBe(readies[0].request_id);
  });

  /* The re-ask must not become a loop. Rust answers `Ready` with `hello` + `command_result`, never
     with another `error`, so a second error can only come from a second real failure -- and each one
     gets exactly one re-ask. */
  it("re-asks once per error rather than compounding", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "error", message: "first" });
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "error", message: "second" });
    expect(posted.filter((m) => m.type === "ready")).toHaveLength(3);
  });

  it("can be dismissed without resurrecting the session", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "error", message: "boom" });
    fireEvent.click(buttonLabelled(container, "Dismiss")!);
    expect(container.querySelector(".fatal-error")).toBeNull();
    expect(container.querySelector(".mode-selector")).not.toBeNull();
  });
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
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState(overrides) });
    return rendered;
  }
  function events(...list: AgentDomainEvent[]) {
    dispatch({ kind: "events", fromRevision: 0, throughRevision: list.length, events: list });
  }
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const press = (key: string, over: Partial<{ shiftKey: boolean }> = {}) =>
    fireEvent.keyDown(document.activeElement ?? document.body, { key, ...over });
  const overlay = (c: HTMLElement) => c.querySelector<HTMLElement>(".keymap-overlay");

  it("opens on ? and shows all four groups, in the spec's order", () => {
    const { container } = started();
    act(() => root(container).focus());
    expect(overlay(container)).toBeNull();
    // `?` almost always arrives as Shift+/ -- the same reason `resolveKey`'s own test checks it
    // both ways (`keymap.test.ts`).
    press("?", { shiftKey: true });
    const el = overlay(container);
    expect(el).not.toBeNull();
    const titles = Array.from(el!.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual(["This panel", "Typing", "Anywhere in the window", "After Ctrl+a"]);
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

/* Change B of the 2026-09-24 fix: `MessageList` ends following on what the user did, and for the
   panel's own scroll keys that is this announcement, made on the list before the key scrolls it.
   Without it, `k` would still end following only through the direction of its scroll -- the very
   signal an engine-originated drop once faked (see `./follow.ts`). */
describe("the panel's own scroll keys announce themselves to the message list", () => {
  function startedApp() {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({
      kind: "snapshot",
      throughRevision: 3,
      state: snapshotState({
        transcript: [
          { seq: 0, text: "one" },
          { seq: 1, text: "two" },
          { seq: 2, text: "three" },
        ],
      }),
    });
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
});
