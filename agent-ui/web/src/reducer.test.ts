import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import type { AgentDomainEvent, TurnOutcome } from "./types";

describe("applyEvent", () => {
  it("session_opened populates identity and sets status to running", () => {
    const event: AgentDomainEvent = { type: "session_opened", session_id: "abc-123", provider_session_id: "claude-abc-123", model: "claude-sonnet-5", cwd: "/tmp" };
    const state = applyEvent(initialState(), event);
    expect(state.sessionId).toBe("abc-123");
    expect(state.model).toBe("claude-sonnet-5");
    expect(state.status).toEqual({ kind: "running" });
  });

  it("turn_started sets activeTurnId", () => {
    const state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    expect(state.activeTurnId).toBe("t1");
  });

  it("content_delta accumulates consecutive text into one message, ignoring thinking deltas", () => {
    // This asserted ["first", "second"] until partial streaming landed. `transcript` holds assistant
    // MESSAGES, and two consecutive text deltas with nothing between them are one message -- under
    // partial streaming they are two fragments of one sentence.
    let state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "text", text: "first" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "pondering" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "second" });
    expect(state.transcript).toEqual(["firstsecond"]);
  });

  it("tool_call_started then tool_call_completed links by tool_use_id", () => {
    let state = applyEvent(initialState(), { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "echo hi" } });
    expect(state.toolCalls).toHaveLength(1);
    expect(state.toolCalls[0].result).toBeNull();
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "hi", is_error: false });
    expect(state.toolCalls[0].result).toEqual({ content: "hi", isError: false });
  });

  it("turn_started sets activeTurnId, and a subsequent turn_completed clears it regardless of outcome", () => {
    // Parametrized over all four TurnOutcome values, mirroring Task 1's own Rust test of the same
    // property (turn_completed always clears the active turn, no matter how it ended).
    const outcomes: TurnOutcome[] = ["completed", "interrupted", "failed", "limit_reached"];
    for (const outcome of outcomes) {
      let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
      expect(state.activeTurnId).toBe("t1");
      state = applyEvent(state, {
        type: "turn_completed", turn_id: "t1", outcome, result_text: "done",
        stop_reason: "end_turn", total_cost_usd: 0.01, num_turns: 1,
      });
      expect(state.activeTurnId).toBeNull();
    }
  });

  it("turn_completed does not change status (v2 semantics: only session_unavailable/session_closed do)", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, {
      type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done",
      stop_reason: "end_turn", total_cost_usd: 0.01, num_turns: 1,
    });
    expect(state.status).toEqual({ kind: "running" });
  });

  it("session_unavailable sets status to unavailable with a reason", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "session_unavailable", reason: "provider process exited unexpectedly" });
    expect(state.status).toEqual({ kind: "unavailable", reason: "provider process exited unexpectedly" });
  });

  it("session_closed sets status to closed with a reason", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "session_closed", reason: "window closed" });
    expect(state.status).toEqual({ kind: "closed", reason: "window closed" });
  });

  /* The forbidden failure mode, at the reducer level: a session that dies mid-turn used to leave
     activeTurnId set forever, so App.tsx kept turnInProgress true, the composer stayed disabled
     with a spinner, and the truncated reply above it looked like a reply that had simply finished
     being short. */
  it("session_unavailable mid-turn clears activeTurnId, because no turn_completed is ever coming", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "half an ans" });
    expect(state.activeTurnId).toBe("t1");

    state = applyEvent(state, { type: "session_unavailable", reason: "3 event(s) were never delivered" });

    expect(state.activeTurnId).toBeNull();
    expect(state.status).toEqual({ kind: "unavailable", reason: "3 event(s) were never delivered" });
    // And the half-written text is still there: clearing the turn must not also erase what did
    // arrive. The banner says it may be incomplete; the reducer does not quietly delete it.
    expect(state.transcript[state.transcript.length - 1]).toBe("half an ans");
  });

  it("session_closed mid-turn clears activeTurnId too", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "session_closed", reason: "closed_by_host" });
    expect(state.activeTurnId).toBeNull();
  });

  it("permission_requested pushes onto pendingPermissions without clearing prior entries", () => {
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r2", tool_name: "Read", input: {} });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r1", "r2"]);
  });

  it("permission_resolved removes only the matching permission, leaving others pending", () => {
    // The direct replacement for the deleted markPermissionAnswered test -- this is now a real
    // event pushed from Rust the instant AgentSession::respond_permission (or interrupt())
    // resolves the request, not a frontend-local optimistic guess.
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r2", tool_name: "Read", input: {} });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "r1", outcome: "allowed" });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r2"]);
  });

  it("permission_resolved removes the matching permission regardless of outcome", () => {
    const outcomes: Array<"allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close"> = [
      "allowed", "denied", "cancelled_by_interrupt", "cancelled_by_session_close",
    ];
    for (const outcome of outcomes) {
      let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_name: "Bash", input: {} });
      state = applyEvent(state, { type: "permission_resolved", permission_id: "r1", outcome });
      expect(state.pendingPermissions).toEqual([]);
    }
  });

  it("permission_resolved on an unknown permission id is a harmless no-op", () => {
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "does-not-exist", outcome: "allowed" });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r1"]);
  });

  it("unrecognized event shapes do not throw and are observable, not silently dropped", () => {
    const consoleWarn = console.warn;
    let warned = false;
    console.warn = () => { warned = true; };
    // @ts-expect-error -- deliberately testing a shape TypeScript wouldn't normally allow through
    const state = applyEvent(initialState(), { type: "some_future_event_type" });
    expect(state).toBeTruthy();
    expect(warned).toBe(true);
    console.warn = consoleWarn;
  });

  it("applySnapshot replaces the whole state wholesale", () => {
    const snapshot = { ...initialState(), sessionId: "replaced", transcript: ["from snapshot"] };
    const state = applySnapshot(initialState(), snapshot);
    expect(state).toEqual(snapshot);
  });
});

describe("identity handling", () => {
  it("keeps the Verdandi session id and the Claude provider session id in separate fields", () => {
    // The two are genuinely different values on the sidecar backend. Measured against a real
    // sidecar: CreateSessionResponse returned one UUID while SessionReady carried another. Storing
    // both in one field would eventually send Claude's id where Verdandi's belongs.
    const state = applyEvent(initialState(), {
      type: "session_opened",
      session_id: "8b3be45a-330b-429f-b1a9-573e73448e23",
      provider_session_id: "f6fabf0a-8be9-4916-879c-ceadc863620e",
      model: "claude-sonnet-5",
      cwd: "/tmp",
    });
    expect(state.sessionId).toBe("8b3be45a-330b-429f-b1a9-573e73448e23");
    expect(state.providerSessionId).toBe("f6fabf0a-8be9-4916-879c-ceadc863620e");
    expect(state.sessionId).not.toBe(state.providerSessionId);
  });

  it("a second session_opened on the same session is idempotent, not a new session", () => {
    // The Agent SDK emits system/init at the start of EVERY turn inside one streaming session, so
    // this event legitimately repeats. It must not reset accumulated state.
    const opened: AgentDomainEvent = {
      type: "session_opened",
      session_id: "sess-1",
      provider_session_id: "claude-1",
      model: "claude-sonnet-5",
      cwd: "/tmp",
    };
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "hello" });
    state = applyEvent(state, opened);
    expect(state.transcript).toEqual(["hello"]);
    expect(state.sessionId).toBe("sess-1");
    expect(state.providerSessionId).toBe("claude-1");
  });

  it("starts with every capability false, so a gated control cannot appear before the server says so", () => {
    const state = initialState();
    expect(state.capabilities).toEqual({ resume: false, fork: false, interrupt: false, bypassPermissionMode: false });
    expect(state.provider).toBeNull();
    expect(state.conversationId).toBeNull();
    expect(state.providerSessionId).toBeNull();
  });
});

describe("partial assistant streaming", () => {
  const opened: AgentDomainEvent = {
    type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp",
  };
  const delta = (text: string): AgentDomainEvent => ({ type: "content_delta", turn_id: "t1", kind: "text", text });

  it("accumulates a streamed reply into ONE transcript entry, not one per delta", () => {
    // The defect this pins: under partial streaming a 600-word reply arrives as 400+ deltas. One
    // transcript entry each renders 400 separate bubbles, every fragment markdown-parsed alone --
    // so "`neovibe_" or "**bold" is not valid standalone markdown and the formatting breaks.
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    for (const chunk of ["The ", "quick ", "**brown** ", "fox"]) state = applyEvent(state, delta(chunk));
    expect(state.transcript).toEqual(["The quick **brown** fox"]);
  });

  it("starts a new entry after a tool call, because that ends the assistant message", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("I'll check."));
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, delta("It printed hi."));
    expect(state.transcript).toEqual(["I'll check.", "It printed hi."]);
  });

  it("starts a new entry on the next turn", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("first"));
    state = applyEvent(state, { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "first", stop_reason: null, total_cost_usd: 0, num_turns: 0 });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    state = applyEvent(state, delta("second"));
    expect(state.transcript).toEqual(["first", "second"]);
  });

  it("a thinking delta does not split the text around it", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("before "));
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    state = applyEvent(state, delta("after"));
    expect(state.transcript).toEqual(["before after"]);
  });

  it("a snapshot resets the open-message flag rather than appending into a closed message", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, delta("streaming"));
    expect(state.assistantMessageOpen).toBe(true);
    const restored = applySnapshot(state, { ...state, transcript: ["an earlier reply"], assistantMessageOpen: true });
    expect(restored.assistantMessageOpen).toBe(false);
    const next = applyEvent(restored, delta("new message"));
    expect(next.transcript).toEqual(["an earlier reply", "new message"]);
  });
});
