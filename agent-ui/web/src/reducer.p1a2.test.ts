import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot } from "./types";
import fixture from "./fixtures/p1a2-mid-stream.json";

/**
 * Codex audit P1-A2 (CONFIRMED): on the sidecar backend, text already inside a tab-switch or
 * reload snapshot could be delivered again by the very next pump, so the reply text rendered
 * twice. The fix is Rust-side and this reducer is unchanged: every queued event carries the
 * projection revision its fold produced, the snapshot's own revision is noted on the backend
 * (`AgentBackend::note_ui_snapshot`), and `TabSet::pump` leaves every event at or below it out of
 * the next `events` payload (`core/src/tab_set.rs`).
 *
 * The fixture is not hand-typed. It is written by the Rust test
 * `core/src/tab_set.rs`, `tests::with_the_filter_off_the_tick_repeats_the_snapshot_and_the_reducer_fixture_records_both`
 * (its own `writtenBy` field says so too), which compares it on every run and rewrites it only
 * under `NEOVIBE_WRITE_FIXTURES=1` when it differs. For each of the two shapes -- `midStream`, the
 * audit's (text folded with no tick since the last one), and `foldAfterADrain`, round 2's (text
 * folded right after a tick's drain, before the snapshot read) -- it holds the real snapshot that
 * `TabSet::active_state_payloads` sent, the real `events` of the tick after it (`nextEvents`), and
 * what that same tick sent with the filter turned off, the semantics before round 2
 * (`nextEventsUnfiltered`).
 */
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

    // What makes the case above a test of anything: the reducer itself does not deduplicate, so the
    // tick as it was before round 2 renders the text twice. If this ever reads 1, the fixture no
    // longer records the race (or the reducer began hiding it), and the case above proves nothing.
    it(`${name}: the same tick without the Rust filter renders the text twice`, () => {
      expect(renderedCount(testCase, testCase.nextEventsUnfiltered)).toBe(2);
    });
  }

  /**
   * Round 1 (Codex, test hygiene), still hand-built so it stays meaningful whatever the fixture
   * holds: a mid-stream snapshot can legitimately be followed by real queued events -- text folded
   * after the switch/reload snapshot was read -- and the reducer must append that as a continuation
   * of the still-open message, never as a duplicate or a second bubble (the same rule `MUST stay
   * identical to AgentSessionProjection::apply` documents in `reducer.ts`'s own `content_delta` case).
   */
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
