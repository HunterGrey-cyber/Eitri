import { describe, expect, it } from "vitest";
import { durationsFromSnapshot, durationsWith, endingDuration, formatDuration, NO_TURN_DURATIONS, workedFooters } from "./turnTimes";
import { turnEndingText } from "./turnEnding";
import type { TurnEnding, TurnTimeWire } from "./types";

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
