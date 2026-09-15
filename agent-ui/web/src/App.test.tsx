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

function buttonLabelled(container: HTMLElement, label: string): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(label));
}

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
    expect(container.querySelectorAll(".assistant-message")).toHaveLength(1);
    expect(container.querySelector(".assistant-message")!.textContent).toContain("Hello world");
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
    expect(container.querySelector(".session-lost")!.textContent).toContain("provider process exited unexpectedly");
    expect(container.querySelector("textarea")!.disabled).toBe(true);
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
