/* The scripted Claude scenarios as the panel folds them. Each file in `fixtures/claude-scripted-*.json` is the
 * events of a scenario the harness recorded (with every string the CLI wrote reduced to a digest), the events
 * payload Rust sends for them, and the snapshot Rust sends after folding the same events
 * (`core/src/agent_bridge.rs`, `the_claude_scripted_twins_are_what_the_serializer_sends`). The reducer must
 * reach what the snapshot carries. */
import { describe, expect, it } from "vitest";
import { applyEvent, applyEvents, applySnapshot, initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState } from "./types";

type Twin = {
  scenario: string;
  domain_events: AgentDomainEvent[];
  events: { events: AgentDomainEvent[]; throughRevision: number };
  snapshot: { state: AgentUiSnapshot; throughRevision: number };
};

const twins = import.meta.glob<Twin>("./fixtures/claude-scripted-*.json", { eager: true, import: "default" });

/** What the fold and the snapshot must agree on: everything the panel draws from a conversation. */
const KEYS = [
  "userPrompts",
  "transcript",
  "toolCalls",
  "activeTurnId",
  "pendingPermissions",
  "availableCommands",
  "turnEndings",
  "lastTurnEnding",
  "assistantMessageOpen",
  "thinking",
  "thinkingOpen",
  "backgroundCalls",
  "plans",
] as const;

const agrees = (state: AgentUiState, twin: Twin) => {
  const final = twin.snapshot.state as unknown as Record<string, unknown>;
  /* A snapshot leaves `plans` out while there are none; the fold keeps an empty list. */
  const sent = (key: (typeof KEYS)[number]) => (key === "plans" ? (final.plans ?? []) : final[key]);
  for (const key of KEYS) {
    expect((state as unknown as Record<string, unknown>)[key], key).toEqual(sent(key));
  }
  expect(state.nextSeq).toBe(twin.snapshot.throughRevision);
};

describe("scripted Claude scenarios", () => {
  it("has at least one twin", () => {
    expect(Object.keys(twins).length).toBeGreaterThan(0);
  });

  for (const [file, twin] of Object.entries(twins)) {
    describe(file, () => {
      it("sends the events it records", () => {
        expect(twin.events.events).toEqual(twin.domain_events);
      });
      it("folded event by event reaches the snapshot", () => {
        agrees(twin.events.events.reduce(applyEvent, initialState()), twin);
      });
      it("folded as one batch reaches the snapshot", () => {
        agrees(applyEvents(initialState(), twin.events.events), twin);
      });
      it("the snapshot loads as it was sent", () => {
        agrees(applySnapshot(initialState(), twin.snapshot.state, twin.snapshot.throughRevision), twin);
      });
      it("names no home directory", () => {
        expect(JSON.stringify(twin.domain_events)).not.toMatch(/\/home\/|\/Users\//);
      });
    });
  }
});
