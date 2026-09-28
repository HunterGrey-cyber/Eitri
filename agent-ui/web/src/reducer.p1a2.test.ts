import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot } from "./types";
import fixture from "./fixtures/p1a2-mid-stream.json";

/** Text already included in a tab-switch or reload snapshot must not arrive again in the next
 * pump. Queued events carry their projection revision; `AgentBackend::note_ui_snapshot` saves
 * the snapshot revision, and `TabSet::pump` excludes events at or below it. The fixture comes
 * from a Rust test in `core/src/tab_set.rs` and contains both filtered and unfiltered events.
 * `midStream` covers text folded without an intervening tick; `foldAfterADrain` covers text
 * folded after a drain but before the snapshot read. Each case holds the actual snapshot,
 * `nextEvents`, and `nextEventsUnfiltered`, allowing the reducer test to detect duplicate text
 * across the language boundary. */
interface P1a2Case {
  text: string;
  snapshot: AgentUiSnapshot;
  throughRevision: number;
  nextEvents: AgentDomainEvent[];
  nextEventsUnfiltered: AgentDomainEvent[];
}

const p1a2Fixture = fixture as unknown as {
  writtenBy: string;
  midStream: P1a2Case;
  foldAfterADrain: P1a2Case;
};

/** How many times `text` appears in the transcript after the snapshot and then `events`. */
function renderedCount(testCase: P1a2Case, events: AgentDomainEvent[]): number {
  let state = applySnapshot(initialState(), testCase.snapshot, testCase.throughRevision);
  for (const event of events) {
    state = applyEvent(state, event);
  }
  const rendered = state.transcript.map((message) => message.text).join("|");
  return rendered.split(testCase.text).length - 1;
}

describe("P1-A2: a mid-stream snapshot is never followed by a repeat of its own text", () => {
  it("names the Rust test that writes its fixture", () => {
    expect(p1a2Fixture.writtenBy).toContain("core/src/tab_set.rs");
  });

  for (const [name, testCase] of [
    ["midStream", p1a2Fixture.midStream],
    ["foldAfterADrain", p1a2Fixture.foldAfterADrain],
  ] as const) {
    it(`${name}: the snapshot carries the text once, and the tick after it adds no second copy`, () => {
      expect(renderedCount(testCase, [])).toBe(1);
      expect(renderedCount(testCase, testCase.nextEvents)).toBe(1);
    });

    // Without the Rust filter, the same tick renders the text twice because the reducer does not
    // deduplicate. If this reads 1, the fixture no longer captures the race or the reducer hides
    // it, so the preceding test no longer proves the filter works.
    it(`${name}: the same tick without the Rust filter renders the text twice`, () => {
      expect(renderedCount(testCase, testCase.nextEventsUnfiltered)).toBe(2);
    });
  }

  /** Keep this case hand-built so it remains useful independently of the fixture. New text folded
   * after a mid-stream snapshot must append to the snapshot's open message without duplicating
   * it or creating a second bubble. This must match `AgentSessionProjection::apply`, like the
   * `content_delta` case in `reducer.ts`. */
  it("appends a genuinely new delta to the snapshot's own open message, not a second one", () => {
    const snapshot: AgentUiSnapshot = {
      ...initialState(),
      status: { kind: "running" },
      activeTurnId: "t1",
      assistantMessageOpen: true,
      transcript: [{ seq: 1, text: "partial " }],
    };
    let state = applySnapshot(initialState(), snapshot, 2);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "continued" });

    expect(state.transcript).toEqual([{ seq: 1, text: "partial continued" }]);
  });
});
