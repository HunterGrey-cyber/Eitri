import { describe, expect, it } from "vitest";
import { stripEntries } from "./whichKey";
import type { StripEntry } from "./whichKey";
import type { AnswerableItem } from "./nav";
import type { TimelineItem } from "./timeline";

function toolItem(seq: number, toolUseId: string, result: { content: unknown; isError: boolean } | null): TimelineItem {
  return { kind: "tool", seq, key: `t-${seq}`, call: { seq, toolUseId, name: "Bash", input: {}, result } };
}
function otherItem(seq: number): TimelineItem {
  return { kind: "message", seq, key: `m-${seq}`, text: "hi" };
}

const KEYS_ONLY = (entries: StripEntry[]) => entries.map((e) => e.key);

describe("stripEntries", () => {
  it("lists nothing for an ordinary row", () => {
    expect(stripEntries([otherItem(1)], [{ kind: "other" }], 0, false)).toEqual([]);
  });

  it("offers allow/deny/buttons on a pending permission under the cursor", () => {
    const answerable: AnswerableItem[] = [{ kind: "permission", toolUseId: "toolu_1" }];
    expect(stripEntries([otherItem(1)], answerable, 0, false)).toEqual([
      { key: "a", label: "allow" },
      { key: "d", label: "deny" },
      { key: "l", label: "buttons" },
    ]);
  });

  /** P1 (ruling 26): with exactly one card anywhere in the conversation, the strip agrees with what
   *  `a`/`d` now do even when the cursor sits on neither the card nor the tool call it gates. */
  it("offers allow/deny/buttons for the only card in the conversation, from any row", () => {
    const answerable: AnswerableItem[] = [{ kind: "other" }, { kind: "permission", toolUseId: "toolu_1" }];
    expect(KEYS_ONLY(stripEntries([otherItem(1), otherItem(2)], answerable, 0, false))).toEqual(["a", "d", "l"]);
  });

  it("offers allow/deny/buttons from the tool call the card gates, too", () => {
    const answerable: AnswerableItem[] = [
      { kind: "tool", toolUseId: "toolu_1" },
      { kind: "permission", toolUseId: "toolu_1" },
    ];
    expect(KEYS_ONLY(stripEntries([otherItem(1), otherItem(2)], answerable, 0, false))).toEqual(["a", "d", "l"]);
  });

  it("never offers allow/deny once the session has ended, even on a card that is still on screen -- only r", () => {
    const answerable: AnswerableItem[] = [{ kind: "permission", toolUseId: "toolu_1" }];
    expect(stripEntries([otherItem(1)], answerable, 0, true)).toEqual([{ key: "r", label: "new session" }]);
  });

  it("offers Enter result on a tool row that already has a result", () => {
    const timeline = [toolItem(1, "toolu_1", { content: "ok", isError: false })];
    expect(stripEntries(timeline, [{ kind: "tool", toolUseId: "toolu_1" }], 0, false)).toEqual([
      { key: "Enter", label: "result" },
    ]);
  });

  it("offers nothing on a tool row that is still running (no result yet)", () => {
    const timeline = [toolItem(1, "toolu_1", null)];
    expect(stripEntries(timeline, [{ kind: "tool", toolUseId: "toolu_1" }], 0, false)).toEqual([]);
  });

  /** GUI pass, 2026-09-25: with the keys on the status row of a LIVE session the strip read
   *  `r new session`, because the edge was passed as if it were a dead session. `r` does nothing
   *  there (`resolveKey` is given the real `sessionEnded`). At an edge the row's own keys go, and
   *  `r` stays exactly as true as the session's end. */
  it("at an edge, drops the row's own keys but does not offer r on a live session", () => {
    const answerable: AnswerableItem[] = [{ kind: "permission", toolUseId: "toolu_1" }];
    const timeline = [toolItem(1, "toolu_1", { content: "ok", isError: false })];
    expect(stripEntries(timeline, answerable, 0, false, true)).toEqual([]);
    expect(stripEntries([otherItem(1)], [{ kind: "other" }], 0, true, true)).toEqual([{ key: "r", label: "new session" }]);
  });

  it("offers r new session once the session has ended", () => {
    expect(stripEntries([otherItem(1)], [{ kind: "other" }], 0, true)).toEqual([{ key: "r", label: "new session" }]);
  });

  it("combines a tool result with the card that gates it -- both groups on one row", () => {
    const timeline = [toolItem(1, "toolu_1", { content: "ok", isError: false }), otherItem(2)];
    const answerable: AnswerableItem[] = [
      { kind: "tool", toolUseId: "toolu_1" },
      { kind: "permission", toolUseId: "toolu_1" },
    ];
    expect(KEYS_ONLY(stripEntries(timeline, answerable, 0, false))).toEqual(["a", "d", "l", "Enter"]);
  });

  it("does not go past the end of the timeline", () => {
    expect(stripEntries([], [], 0, false)).toEqual([]);
  });

  /* P2 (Task 13): a collapsed run is a row with its own Enter action -- expand it -- distinct from
     an ordinary finished tool row, which offers to fold/unfold its own result instead. */
  it("offers Enter expand on a collapsed run", () => {
    const run: TimelineItem = { kind: "run", seq: 1, key: "r-1", calls: [] };
    expect(stripEntries([run], [{ kind: "other" }], 0, false)).toEqual([{ key: "Enter", label: "expand" }]);
  });
});
