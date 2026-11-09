import { describe, expect, it } from "vitest";
import { durationsFromSnapshot, durationsWith, endingDuration, formatDuration, NO_TURN_DURATIONS, placeFooters, workedFooters } from "./turnTimes";
import { turnEndingText } from "./turnEnding";
import { applyEvent, applySnapshot, initialState, withEditorCards } from "./reducer";
import { buildTimeline, type TimelineItem } from "./timeline";
import type { AgentUiSnapshot, EditorCardWire, Seq, TurnEnding, TurnTimeWire } from "./types";

const wire = (turnId: string, durationMs: number, anchorSeq: number | null = 5): TurnTimeWire => ({
  turnId,
  startedAtMs: 1_000_000,
  endedAtMs: 1_000_000 + durationMs,
  anchorSeq,
});

describe("formatDuration", () => {
  it.each([
    [0, "0s"],
    [8_000, "8s"],
    [8_999, "8s"],
    [59_999, "59s"],
    [60_000, "1m 00s"],
    [83_000, "1m 23s"],
    [605_000, "10m 05s"],
    [3_599_000, "59m 59s"],
    [3_600_000, "1h 00m"],
    [3_720_000, "1h 02m"],
    [36_000_000 + 59 * 60_000 + 59_000, "10h 59m"],
    [-5, "0s"],
  ])("%d ms reads %s", (ms, text) => {
    expect(formatDuration(ms)).toBe(text);
  });
});

describe("the duration table", () => {
  it("a snapshot's list replaces the table whole; a sender without the field reads as empty", () => {
    const first = durationsFromSnapshot([wire("t1", 5_000), wire("t2", 7_000)]);
    expect([...first.keys()]).toEqual(["t1", "t2"]);
    expect(first.get("t2")).toEqual({ durationMs: 7_000, anchorSeq: 5 });
    expect(durationsFromSnapshot(undefined).size).toBe(0);
  });

  it("an events list adds to it, and an empty or missing one changes nothing", () => {
    const current = durationsFromSnapshot([wire("t1", 5_000)]);
    expect(durationsWith(current, undefined)).toBe(current);
    expect(durationsWith(current, [])).toBe(current);
    expect([...durationsWith(current, [wire("t2", 9_000)]).keys()]).toEqual(["t1", "t2"]);
  });

  it("an end before its start reads as no time, never a negative one", () => {
    const table = durationsFromSnapshot([{ turnId: "t1", startedAtMs: 10, endedAtMs: 5, anchorSeq: 1 }]);
    expect(table.get("t1")?.durationMs).toBe(0);
  });

  it("keeps the newest 1000 of 1200 received times, in receipt order rather than timestamp order", () => {
    const times = Array.from({ length: 1200 }, (_, i) => ({ ...wire(`t${i}`, i), startedAtMs: 10_000 - i }));
    const expected = times.slice(200).map((time) => time.turnId);
    const current = durationsFromSnapshot(times.slice(0, 900));
    const merged = durationsWith(current, times.slice(900));
    expect([...merged.keys()]).toEqual(expected);
    expect(current.size).toBe(900);
    expect(merged.get("t1199")).toEqual({ durationMs: times[1199].endedAtMs - times[1199].startedAtMs, anchorSeq: 5 });
    expect([...durationsWith(NO_TURN_DURATIONS, times).keys()]).toEqual(expected);
    expect([...durationsFromSnapshot(times).keys()]).toEqual(expected);
  });
});

describe("workedFooters", () => {
  const ending = (turnId: string): TurnEnding => ({
    seq: 9,
    turnId,
    kind: "interrupted",
    reason: null,
    apiErrorStatus: null,
    message: null,
  });

  /** A state whose agent wrote rows at `seqs` (replies, here; any kind Rust's anchor names will do). */
  const rows = (activeTurnId: string | null, turnEndings: TurnEnding[], ...seqs: number[]) => ({
    activeTurnId,
    turnEndings,
    transcript: seqs.map((seq) => ({ seq, text: "r" })),
    thinking: [],
    toolCalls: [],
    pendingPermissions: [],
  });

  it("words each finished turn under the row it ended on", () => {
    const table = durationsFromSnapshot([wire("t1", 42_000, 10), wire("t2", 65_000, 17)]);
    const footers = workedFooters(rows(null, [], 10, 17), table);
    expect(footers.get(10)).toBe("Worked for 42s");
    expect(footers.get(17)).toBe("Worked for 1m 05s");
  });

  it("leaves out the running turn, a turn that ended badly, an unplaced turn, and a second turn on one row", () => {
    const table = durationsFromSnapshot([
      wire("t1", 1_000, 10),
      wire("t2", 2_000, 20),
      wire("t3", 3_000, null),
      wire("t4", 4_000, 10),
      wire("t5", 5_000, 30),
    ]);
    const footers = workedFooters(rows("t5", [ending("t2")], 10, 20, 30), table);
    expect([...footers]).toEqual([[10, "Worked for 1s"]]);
  });

  it("leaves out a turn whose row is not in the state, and names a thought as a row", () => {
    const table = durationsFromSnapshot([wire("t1", 1_000, 10), wire("t2", 2_000, 99), wire("t3", 3_000, 12)]);
    const state = { ...rows(null, [], 10), thinking: [{ seq: 12, turnId: "t3", text: "done" }] };
    expect([...workedFooters(state, table)]).toEqual([
      [10, "Worked for 1s"],
      [12, "Worked for 3s"],
    ]);
  });

  it("is empty when no turn has a known time", () => {
    expect(workedFooters(rows(null, [], 1), NO_TURN_DURATIONS).size).toBe(0);
  });

  /* A broker card is numbered from the tab's revision without taking one: the page's counter stays Rust's, so the
     next reply folded gets the number Rust gave it and the turn's footer, anchored on that number, still finds it. */
  it("a_turns_footer_still_places_after_an_editor_card", () => {
    const card: EditorCardWire = {
      id: "ec-0123456789abcdef-1", seq: 12, tool: "open_file", toolUseId: null, title: "Show a?", body: [{ kind: "plain", text: "x" }],
    };
    const snapshot = { ...initialState(), transcript: [{ seq: 10, text: "before" }], editorCards: [card] } as unknown as AgentUiSnapshot;
    let state = applySnapshot(initialState(), snapshot, 12);
    expect(state.editorCards.map((c) => c.permissionId)).toEqual([card.id]);
    state = withEditorCards(state, [card, { ...card, id: "ec-0123456789abcdef-2", seq: 12 }]);
    expect(state.nextSeq).toBe(12);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "after" });
    expect(state.transcript.map((m) => m.seq)).toEqual([10, 12]);
    const footers = workedFooters(state, durationsFromSnapshot([wire("t1", 5_000, 12)]));
    expect([...footers]).toEqual([[12, "Worked for 5s"]]);
    expect(placeFooters(buildTimeline(state), footers).get("m-12")).toBe("Worked for 5s");
  });

  /* The card's number is the revision the next event takes, so a reply may share it; the footer is the reply's. */
  it("a_footer_anchored_on_a_number_a_card_shares_stands_under_the_reply", () => {
    const card: EditorCardWire = {
      id: "ec-0123456789abcdef-1", seq: 12, tool: "open_file", toolUseId: "toolu_c", title: "Show a?", body: [{ kind: "plain", text: "x" }],
    };
    let state = withEditorCards(initialState(), [card]);
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_c", name: "mcp__eitri__open_file", input: {} });
    state = { ...state, nextSeq: 12 };
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "after" });
    const items = buildTimeline(state);
    expect(items.map((item) => item.key)).toContain("ec-ec-0123456789abcdef-1");
    const footers = workedFooters(state, durationsFromSnapshot([wire("t1", 5_000, 12)]));
    const placed = placeFooters(items, footers);
    expect([...placed.keys()]).toEqual(["m-12"]);
  });
});

describe("a turn-ending row's duration", () => {
  it("is worded when known and absent when not", () => {
    const table = durationsFromSnapshot([wire("t1", 12_000)]);
    expect(endingDuration(table, "t1")).toBe("12s");
    expect(endingDuration(table, "t9")).toBeNull();
  });

  it("follows the headline and comes before the provider's own words", () => {
    const interrupted = { kind: "interrupted", reason: null, apiErrorStatus: null, message: null } as const;
    expect(turnEndingText(interrupted, null, "12s")).toBe("interrupted after 12s");
    expect(turnEndingText(interrupted)).toBe("interrupted");
    const failed = { kind: "failed", reason: "api_error", apiErrorStatus: 529, message: "Overloaded" } as const;
    expect(turnEndingText(failed, null, "1m 23s")).toBe("the turn ended with an error (HTTP 529) after 1m 23s: Overloaded");
    const lost = { kind: "lost", reason: null, apiErrorStatus: null, message: null } as const;
    expect(turnEndingText(lost, null, "3s")).toBe("the turn did not finish: the session ended after 3s");
  });
});

function oldPlaceFooters(items: readonly TimelineItem[], footers: ReadonlyMap<Seq, string>): ReadonlyMap<string, string> {
  const out = new Map<string, string>();
  if (footers.size === 0) return out;
  const anchors = [...footers.keys()].sort((a, b) => a - b);
  // Each row by every `seq` it holds, the first row to hold one: a turn's line is looked up once per turn, and
  // walking the rows for each of them is a turn count times a row count on every streamed batch.
  const holders = new Map<Seq, TimelineItem>();
  const hold = (seq: Seq, item: TimelineItem) => {
    if (!holders.has(seq)) holders.set(seq, item);
  };
  for (const item of items) {
    if ((item.kind === "permission" && item.request.editorCard !== undefined)) continue;
    if (item.kind !== "run") {
      hold(item.seq, item);
      continue;
    }
    for (const call of item.calls) hold(call.seq, item);
    for (const thought of item.thoughts ?? []) hold(thought.seq, item);
  }
  const left: Seq[] = [];
  for (const anchor of anchors) {
    const at = holders.get(anchor);
    if (at === undefined) left.push(anchor);
    else if (!out.has(at.key)) out.set(at.key, footers.get(anchor)!);
  }
  for (const anchor of left) {
    let under: TimelineItem | null = null;
    for (const item of items) {
      if (item.kind === "prompt" && item.seq > anchor) break;
      if (item.seq <= anchor && !(item.kind === "permission" && item.request.editorCard !== undefined)) under = item;
    }
    if (under !== null && !out.has(under.key)) out.set(under.key, footers.get(anchor)!);
  }
  return out;
}


describe("footer fallback placement", () => {
  const message = (seq: number): TimelineItem => ({ kind: "message", seq, key: `m-${seq}`, text: "reply" });
  const prompt = (seq: number): TimelineItem => ({ kind: "prompt", seq, key: `u-${seq}`, text: "prompt" });

  it("matches the scan for many hidden anchors, including displayed-order and prompt boundaries", () => {
    const items: TimelineItem[] = [];
    const footers = new Map<number, string>();
    for (let i = 0; i < 500; i++) {
      const seq = i * 10;
      items.push(prompt(seq), message(seq + 1), message(seq + 8), message(seq + 2));
      footers.set(seq + 3, `hidden ${i}`);
      if (i % 3 === 0) footers.set(seq + 1, `direct ${i}`);
      if (i % 5 === 0) footers.set(seq + 4, `second hidden ${i}`);
    }
    // A high prompt stops every older anchor even if later displayed rows have smaller sequences.
    items.splice(300, 0, prompt(9_999));
    expect([...placeFooters(items, footers)]).toEqual([...oldPlaceFooters(items, footers)]);
  });

  it("indexes missing anchors without revisiting timeline sequences per turn", () => {
    let reads = 0;
    const items = Array.from({ length: 500 }, (_, i): TimelineItem => ({
      kind: "message", get seq() { reads++; return i * 2; }, key: `m-${i * 2}`, text: "reply",
    }));
    const footers = new Map(items.map((_, i) => [i * 2 + 1, `turn ${i}`]));
    expect(placeFooters(items, footers).size).toBe(500);
    expect(reads).toBeLessThan(items.length * 12);
  });
});
