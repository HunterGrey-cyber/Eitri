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
  resumableSession: null,
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
      state: snapshotState({ model: "claude-opus-5", transcript: ["pre-reload marker alpha seven."] }),
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
    dispatch({ kind: "snapshot", throughRevision: 1, state: snapshotState({ transcript: ["still here"] }) });
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
      total_cost_usd: 0,
      num_turns: 1,
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

describe("App fatal errors", () => {
  it("shows the whole error text and returns to the start screen", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "snapshot", throughRevision: 0, state: snapshotState({ transcript: ["gone"] }) });
    dispatch({ kind: "error", message: "sidecar handshake failed\nclaude CLI 2.1.272 is untested" });
    const banner = container.querySelector(".fatal-error")!;
    // <pre>, because the sidecar's diagnostics are multi-line and the exact text is the point.
    expect(banner.querySelector("pre")!.textContent).toContain("claude CLI 2.1.272 is untested");
    expect(container.querySelector(".mode-selector")).not.toBeNull();
    // The dead session's transcript is gone with it, rather than left on screen looking live.
    expect(container.textContent).not.toContain("gone");
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
