// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
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
    it("does not run a BROWSE action when the key came from a button", () => {
      const { container } = withAToolCallAndAPermission();
      const clipboard = stubClipboard();
      fireEvent.keyDown(buttonLabelled(container, "Approve")!, { key: "y" });
      expect(clipboard.writeText).not.toHaveBeenCalled();
      // The same key at the panel root still DOES act -- otherwise this would pass with the whole
      // handler deleted.
      fireEvent.keyDown(conversationRoot(container), { key: "y" });
      expect(clipboard.writeText).toHaveBeenCalledWith(JSON.stringify({ cmd: "ls" }, null, 2));
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
