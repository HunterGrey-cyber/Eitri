// @vitest-environment jsdom
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState } from "./types";
import fixture from "./fixtures/history-evicted.json";
import { MessageList } from "./components/MessageList";
import { conversationRows, stopsIn } from "./nav";

afterEach(cleanup);
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });

it("folds the shared eviction fixture with the same surviving sequences and children as Rust", () => {
  const unknown = vi.fn();
  let state = initialState();
  for (const event of fixture.events) state = applyEvent(state, event as AgentDomainEvent, unknown);
  const fields = {
    user_prompts: "userPrompts", transcript: "transcript", thinking: "thinking", tool_calls: "toolCalls",
    turn_starts: "turnStarts", turn_endings: "turnEndings", notices: "notices", questions: "questions",
  } as const;
  for (const [rust, ts] of Object.entries(fields)) {
    expect(state[ts].map((r) => r.seq)).toEqual(fixture.surviving[rust as keyof typeof fields]);
  }
  expect(state.evicted).toEqual({ throughSeq: fixture.evicted.through_seq, turns: fixture.evicted.turns });
  expect(state.toolCalls[0].background).toBe(true);
  expect(state.toolCalls[0].children?.toolCalls[0].toolUseId).toBe("nested");
  expect(unknown).not.toHaveBeenCalled();
  const snapshot = applySnapshot(initialState(), state, state.nextSeq);
  expect(snapshot.evicted).toEqual(state.evicted);
  expect(applySnapshot(snapshot, { ...state, evicted: undefined }, state.nextSeq).evicted).toBeNull();
  expect(applyEvent(state, { type: "conversation_reset", new_conversation_id: "new", trigger: null }).evicted).toBeNull();
});

function list(state: AgentUiState, touch = false) {
  return <MessageList state={state} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={() => {}} touch={touch} />;
}

it("shows a single plain desk line below the restore notice, outside timeline indexes", () => {
  const state = { ...initialState(), evicted: { throughSeq: 4, turns: 1 },
    history: { source: "acp_replay", uptoSeq: 1, restoredItems: 1, omittedItems: 0, omittedTurns: 0, sourcePath: "", fallbackReason: null, attemptedTranscriptPath: null, writerVersion: null },
    userPrompts: [{ seq: 4, text: "live" }],
  } satisfies AgentUiState;
  const { container, rerender } = render(list(state));
  const notices = container.querySelectorAll(".history-notice");
  expect(notices).toHaveLength(2);
  expect(notices[1].textContent).toBe("1 earlier turn not loaded.");
  expect(conversationRows(container)).toHaveLength(1);
  expect(stopsIn(container)).not.toContain(notices[1]);
  expect(notices[1].querySelector("button")).toBeNull();
  rerender(list({ ...state, evicted: { throughSeq: 4, turns: 12 } }));
  expect(container.textContent).toContain("12 earlier turns not loaded.");
  rerender(list({ ...state, evicted: { throughSeq: 4, turns: 0 } }));
  expect(container.querySelectorAll(".history-notice")).toHaveLength(1);
  rerender(list(state, true));
  expect(container.textContent).not.toContain("1 earlier turn not loaded.");
});

