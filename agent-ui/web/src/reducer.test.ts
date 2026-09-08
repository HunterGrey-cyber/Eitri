import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState, markPermissionAnswered, markTurnInterrupted, markTurnStarted } from "./reducer";
import type { AgentEvent } from "./types";

describe("applyEvent", () => {
  it("session_started populates identity and sets status to running", () => {
    const event: AgentEvent = { type: "session_started", session_id: "abc-123", model: "claude-sonnet-5", cwd: "/tmp" };
    const state = applyEvent(initialState(), event);
    expect(state.sessionId).toBe("abc-123");
    expect(state.model).toBe("claude-sonnet-5");
    expect(state.status).toEqual({ kind: "running" });
  });

  it("assistant_text appends to transcript in order", () => {
    let state = applyEvent(initialState(), { type: "assistant_text", text: "first" });
    state = applyEvent(state, { type: "assistant_text", text: "second" });
    expect(state.transcript).toEqual(["first", "second"]);
  });

  it("tool_started then tool_result links by id", () => {
    let state = applyEvent(initialState(), { type: "tool_started", id: "toolu_1", name: "Bash", input: { command: "echo hi" } });
    expect(state.toolCalls).toHaveLength(1);
    expect(state.toolCalls[0].result).toBeNull();
    state = applyEvent(state, { type: "tool_result", id: "toolu_1", content: "hi", is_error: false });
    expect(state.toolCalls[0].result).toEqual({ content: "hi", isError: false });
  });

  it("turn_finished clears turn_in_progress but does not change status (v2 semantics)", () => {
    let state = applyEvent(initialState(), { type: "session_started", session_id: "abc", model: "m", cwd: "/tmp" });
    state = { ...state, turnInProgress: true };
    state = applyEvent(state, { type: "turn_finished", result_text: "done", is_error: false, stop_reason: "end_turn", total_cost_usd: 0.01, num_turns: 1 });
    expect(state.turnInProgress).toBe(false);
    expect(state.status).toEqual({ kind: "running" });
  });

  it("process_exited is the only event that sets status to finished", () => {
    let state = applyEvent(initialState(), { type: "session_started", session_id: "abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "process_exited", success: true });
    expect(state.status).toEqual({ kind: "finished", isError: false });
  });

  it("permission_request pushes onto pendingPermissions without clearing prior entries", () => {
    let state = applyEvent(initialState(), { type: "permission_request", request_id: "r1", tool_name: "Bash", input: {}, source: "hook_relay" });
    state = applyEvent(state, { type: "permission_request", request_id: "r2", tool_name: "Read", input: {}, source: "hook_relay" });
    expect(state.pendingPermissions.map((p) => p.requestId)).toEqual(["r1", "r2"]);
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

  it("markTurnStarted sets turnInProgress optimistically, independent of any AgentEvent", () => {
    // No AgentEvent ever sets turnInProgress to true -- it's local Rust-side bookkeeping the
    // frontend must mirror itself right when it calls send_message, not something that arrives
    // over the event stream. Without this, the Stop button/composer-disabling would be dead.
    const state = markTurnStarted(initialState());
    expect(state.turnInProgress).toBe(true);
  });

  it("a subsequent turn_finished event still correctly clears turnInProgress after markTurnStarted", () => {
    let state = markTurnStarted(initialState());
    expect(state.turnInProgress).toBe(true);
    state = applyEvent(state, {
      type: "turn_finished", result_text: "done", is_error: false,
      stop_reason: "end_turn", total_cost_usd: 0.01, num_turns: 1,
    });
    expect(state.turnInProgress).toBe(false);
  });

  it("markPermissionAnswered removes only the answered request, leaving others pending", () => {
    let state = applyEvent(initialState(), { type: "permission_request", request_id: "r1", tool_name: "Bash", input: {}, source: "hook_relay" });
    state = applyEvent(state, { type: "permission_request", request_id: "r2", tool_name: "Read", input: {}, source: "hook_relay" });
    state = markPermissionAnswered(state, "r1");
    expect(state.pendingPermissions.map((p) => p.requestId)).toEqual(["r2"]);
  });

  it("applySnapshot replaces the whole state wholesale", () => {
    const snapshot = { ...initialState(), sessionId: "replaced", transcript: ["from snapshot"] };
    const state = applySnapshot(initialState(), snapshot);
    expect(state).toEqual(snapshot);
  });

  it("markPermissionAnswered on an unknown request id is a harmless no-op", () => {
    // Mirrors the real duplicate-answer case found in Task 8's sandbox verification: the Rust
    // side already removed the request and logs an error, but the frontend must not throw either.
    let state = applyEvent(initialState(), { type: "permission_request", request_id: "r1", tool_name: "Bash", input: {}, source: "hook_relay" });
    state = markPermissionAnswered(state, "does-not-exist");
    expect(state.pendingPermissions.map((p) => p.requestId)).toEqual(["r1"]);
  });

  it("markTurnInterrupted clears every pending permission, mirroring AgentSession::interrupt's own clear", () => {
    let state = applyEvent(initialState(), { type: "permission_request", request_id: "r1", tool_name: "Bash", input: {}, source: "hook_relay" });
    state = applyEvent(state, { type: "permission_request", request_id: "r2", tool_name: "Read", input: {}, source: "hook_relay" });
    state = markTurnInterrupted(state);
    expect(state.pendingPermissions).toEqual([]);
  });
});
