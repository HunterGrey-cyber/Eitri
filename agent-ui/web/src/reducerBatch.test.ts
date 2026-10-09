import { describe, expect, it } from "vitest";
import { applyEvent, applyEvents, initialState, transcriptEntriesCopiedCount } from "./reducer";
import type { AgentDomainEvent, AgentUiState } from "./types";

/* A delivered batch is folded by `applyEvents`, which must give exactly the state folding its events
   one by one gives (that fold mirrors Rust's projection), while copying the transcript once per run
   of text deltas instead of once per delta. */

function longState(messages: number): AgentUiState {
  const transcript = Array.from({ length: messages }, (_, i) => ({ seq: i, text: `reply ${i}` }));
  return { ...initialState(), transcript, nextSeq: messages, assistantMessageOpen: true, activeTurnId: "t1" };
}
const textDelta = (text: string, turn = "t1"): AgentDomainEvent => ({ type: "content_delta", turn_id: turn, kind: "text", text });
const oneByOne = (state: AgentUiState, events: AgentDomainEvent[]) => events.reduce((acc, e) => applyEvent(acc, e), state);

describe("applyEvents", () => {
  it("copies the transcript once for a batch of text deltas, not once per delta", () => {
    for (const [messages, count] of [[1000, 100], [10000, 300]]) {
      const state = longState(messages);
      const events = Array.from({ length: count }, () => textDelta("w "));
      let before = transcriptEntriesCopiedCount();
      let t0 = performance.now();
      const expected = oneByOne(state, events);
      const perDelta = { copied: transcriptEntriesCopiedCount() - before, ms: performance.now() - t0 };
      before = transcriptEntriesCopiedCount();
      t0 = performance.now();
      const got = applyEvents(state, events);
      const batched = { copied: transcriptEntriesCopiedCount() - before, ms: performance.now() - t0 };
      console.log(
        `[batch] ${count} deltas over ${messages} messages: one by one ${perDelta.copied} entries copied (${perDelta.ms.toFixed(3)} ms), batched ${batched.copied} (${batched.ms.toFixed(3)} ms)`,
      );
      expect(got).toEqual(expected);
      expect(batched.copied).toBe(messages);
    }
  });

  it("folds any mix of events to the same state as one by one", () => {
    // A small deterministic generator, so a failure names a reproducible case.
    let seed = 7;
    const rand = (n: number) => {
      seed = (seed * 1103515245 + 12345) % 2147483648;
      return seed % n;
    };
    const pick = (): AgentDomainEvent => {
      switch (rand(9)) {
        case 0:
          return { type: "turn_started", turn_id: `t${rand(3)}` };
        case 1:
          return { type: "assistant_message_boundary", turn_id: "t1" };
        case 2:
          return { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hm" };
        case 3:
          return { type: "user_prompt_submitted", text: "go" };
        case 4:
          return textDelta("", `t${rand(2)}`);
        default:
          return textDelta(["a", "b ", "`c", "**d", "\n\n"][rand(5)], `t${rand(2)}`);
      }
    };
    for (let round = 0; round < 500; round++) {
      const start = round % 2 === 0 ? initialState() : longState(rand(4));
      const events = Array.from({ length: rand(40) }, pick);
      expect(applyEvents(start, events)).toEqual(oneByOne(start, events));
    }
  });

  it("returns the state it was given for an empty batch", () => {
    const state = longState(3);
    expect(applyEvents(state, [])).toBe(state);
  });
});
