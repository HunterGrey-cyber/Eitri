/* The agent's thinking as the page folds it: one block per run of thinking deltas, ordered by the delta
 * that opened it, closed by a reply and by everything that ends a reply, exactly as Rust's
 * `AgentSessionProjection::apply` folds it. The fixture is written by Rust from its own fold and
 * serializer (`core/src/agent_bridge.rs`, `the_thinking_fixture_is_what_the_serializer_sends`). */
import { describe, expect, it } from "vitest";
import { applyEvent, applyEvents, applySnapshot, initialState, keepAfterFailure } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState } from "./types";
import fixture from "./fixtures/thinking.json";

const thought = (text: string, turn = "t1"): AgentDomainEvent => ({ type: "content_delta", turn_id: turn, kind: "thinking", text });
const said = (text: string, turn = "t1"): AgentDomainEvent => ({ type: "content_delta", turn_id: turn, kind: "text", text });
const fold = (events: AgentDomainEvent[], from: AgentUiState = initialState()) => events.reduce(applyEvent, from);
const texts = (state: AgentUiState) => state.thinking.map((t) => t.text);

describe("thinking blocks", () => {
  it("consecutive deltas are one block that keeps the seq of its first", () => {
    const state = fold([{ type: "turn_started", turn_id: "t1" }, thought("Look at "), thought("the parser"), thought(" first.\nThen the tests.")]);
    expect(state.thinking).toEqual([{ seq: 1, turnId: "t1", text: "Look at the parser first.\nThen the tests." }]);
    expect(state.thinkingOpen).toBe(true);
    expect(state.nextSeq).toBe(4);
  });

  it("a block closes at a reply, a call, a card and every boundary, and the next thought is a block of its own", () => {
    const closers: [string, AgentDomainEvent][] = [
      ["text", said("Here is the plan.")],
      ["message boundary", { type: "assistant_message_boundary", turn_id: "t1" }],
      ["tool call", { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Read", input: {} }],
      ["card", { type: "permission_requested", permission_id: "p1", tool_use_id: null, tool_name: "Bash", input: {} }],
      ["prompt", { type: "user_prompt_submitted", text: "and?" }],
      ["steered message read", { type: "user_message_delivered", client_message_id: "eitri-1-1", turn_id: "t1", text: "and the tests" }],
      ["turn start", { type: "turn_started", turn_id: "t2" }],
      [
        "turn end",
        { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } },
      ],
      ["session closed", { type: "session_closed", reason: "closed_by_host" }],
      ["session lost", { type: "session_unavailable", reason: "the provider exited" }],
    ];
    for (const [name, closer] of closers) {
      let state = fold([{ type: "turn_started", turn_id: "t1" }, thought("first"), closer]);
      expect(state.thinkingOpen, name).toBe(false);
      state = applyEvent(state, thought("second"));
      expect(texts(state), name).toEqual(["first", "second"]);
      expect(state.thinking[0].seq < state.thinking[1].seq, name).toBe(true);
    }
  });

  it("what is not an item of the conversation does not cut a thought in two", () => {
    const state = fold([
      { type: "turn_started", turn_id: "t1" },
      thought("one "),
      { type: "tool_call_completed", turn_id: "t1", tool_use_id: "tu0", content: "ok", is_error: false },
      { type: "account_limits", used_percent: 12, window_minutes: null, resets_at: null, plan: null },
      { type: "permission_resolved", permission_id: "p0", outcome: "allowed" },
      { type: "background_calls", running: 1 },
      { type: "user_message_dropped", client_message_id: "eitri-1-2", reason: "the turn ended before the agent read it" },
      thought("thought"),
    ]);
    expect(texts(state)).toEqual(["one thought"]);
  });

  it("a delta with no text opens no block and closes none", () => {
    let state = fold([thought("")]);
    expect(state.thinking).toEqual([]);
    expect(state.thinkingOpen).toBe(false);
    expect(state.nextSeq).toBe(1);
    state = fold([thought("a"), thought(""), thought("b")], state);
    expect(texts(state)).toEqual(["ab"]);
    expect(state.thinkingOpen).toBe(true);
  });

  it("a thought inside a reply does not split the reply", () => {
    const state = fold([said("Half "), thought("hmm"), said("and half.")]);
    expect(state.transcript.map((m) => m.text)).toEqual(["Half and half."]);
    expect(texts(state)).toEqual(["hmm"]);
    expect(state.thinkingOpen).toBe(false);
  });

  it("a snapshot without the two keys (a payload recorded before them) is a conversation with no thinking", () => {
    const { thinking: _t, thinkingOpen: _o, nextSeq: _n, turnThinking: _b, denialsBeforeTheirCall: _d, ...older } = fold([thought("x")]);
    const restored = applySnapshot(initialState(), older as AgentUiSnapshot, 1);
    expect(restored.thinking).toEqual([]);
    expect(restored.thinkingOpen).toBe(false);
    // And the next thought starts a block of its own.
    expect(texts(applyEvent(restored, thought("y")))).toEqual(["y"]);
  });

  it("a failed tab keeps its thinking on screen but no longer open", () => {
    const state = fold([{ type: "turn_started", turn_id: "t1" }, thought("only a thought")]);
    const kept = keepAfterFailure(state, "the sidecar stopped");
    expect(kept?.thinking).toEqual(state.thinking);
    expect(kept?.thinkingOpen).toBe(false);
  });

  describe("the contract fixture (what Rust sends)", () => {
    for (const c of fixture.cases) {
      const events = c.events.events as AgentDomainEvent[];
      const final = c.snapshot.state as unknown as AgentUiSnapshot;
      const middle = c.middle.state as unknown as AgentUiSnapshot;
      const check = (state: AgentUiState) => {
        expect(state.thinking).toEqual(final.thinking);
        expect(state.thinkingOpen).toBe(final.thinkingOpen);
        expect(state.transcript).toEqual(final.transcript);
        expect(state.assistantMessageOpen).toBe(final.assistantMessageOpen);
        expect(state.toolCalls.map((call) => call.seq)).toEqual(final.toolCalls.map((call) => call.seq));
        expect(state.userPrompts).toEqual(final.userPrompts);
        expect(state.nextSeq).toBe(c.snapshot.throughRevision);
      };
      it(`${c.name}: folding every event one by one gives what the last snapshot carries`, () => {
        check(fold(events));
      });
      it(`${c.name}: folding them as one batch gives the same`, () => {
        check(applyEvents(initialState(), events));
      });
      it(`${c.name}: the middle snapshot and the events after it give the same`, () => {
        const loaded = applySnapshot(initialState(), middle, c.middle.throughRevision);
        expect(loaded.thinking).toEqual(middle.thinking);
        expect(loaded.thinkingOpen).toBe(middle.thinkingOpen);
        check(fold(events.slice(c.split), loaded));
        check(applyEvents(loaded, events.slice(c.split)));
      });
      it(`${c.name}: the last snapshot loads as it was sent`, () => {
        const loaded = applySnapshot(initialState(), final, c.snapshot.throughRevision);
        check(loaded);
      });
    }
  });
});
