import { describe, expect, it } from "vitest";
import { buildTimeline, oldestPendingPermission, promptIndex, waitingCardAfter, waitingCardIndex } from "./timeline";
import type { TimelineItem } from "./timeline";
import { initialState } from "./reducer";
import type { AgentUiState, PermissionRequestRecord, ToolCallRecord, TranscriptMessage } from "./types";

function msg(seq: number, text: string): TranscriptMessage {
  return { seq, text };
}
function tool(seq: number, toolUseId: string): ToolCallRecord {
  return { seq, toolUseId, name: "Bash", input: {}, result: null };
}
function perm(seq: number, permissionId: string, toolUseId: string | null): PermissionRequestRecord {
  return { seq, permissionId, toolUseId, toolName: "Bash", input: {} };
}
function state(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

/** A compact label per item, so a test asserts a whole sequence rather than one index at a time. */
function labels(s: AgentUiState): string[] {
  return buildTimeline(s).map((item) => {
    switch (item.kind) {
      case "prompt":
        return `prompt:${item.text}`;
      case "message":
        return `text:${item.text}`;
      case "tool":
        return `tool:${item.call.toolUseId}`;
      case "permission":
        return `perm:${item.request.permissionId}`;
      case "run":
        // `buildTimeline` itself never produces this kind -- only `display.ts`'s `buildDisplay`
        // does, over `buildTimeline`'s own output -- but `TimelineItem`'s union includes it (Task
        // 13, P2), so this switch has to name it too.
        throw new Error("buildTimeline never produces a run item");
    }
  });
}

describe("buildTimeline", () => {
  /* The defect this closes. Rendering the three collections one after another put every tool card
     below every assistant message, so a turn that went text/tool/text/tool read as text/text/tool/tool
     -- which is not what happened, and is the panel's most visible wrong statement about a turn. */
  it("interleaves messages and tool calls into the order they actually happened", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "I'll check."), msg(3, "And now the other one."), msg(6, "Done.")],
          toolCalls: [tool(2, "toolu_1"), tool(4, "toolu_2")],
        }),
      ),
    ).toEqual(["text:I'll check.", "tool:toolu_1", "text:And now the other one.", "tool:toolu_2", "text:Done."]);
  });

  it("does not depend on the arrays arriving in any particular order themselves", () => {
    // Rust emits them sorted; nothing in the wire type PROMISES that, so the merge sorts rather
    // than trusting it.
    expect(
      labels(
        state({
          transcript: [msg(3, "second"), msg(1, "first")],
          toolCalls: [tool(4, "toolu_2"), tool(2, "toolu_1")],
        }),
      ),
    ).toEqual(["text:first", "tool:toolu_1", "text:second", "tool:toolu_2"]);
  });

  /* A permission card is about one specific tool call, and a turn can have several of the same tool
     in flight -- so when the link exists the card belongs against that call, not merely somewhere
     nearby in time. */
  it("puts a linked permission immediately after the tool call it gates, not at its own seq", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before"), msg(5, "after")],
          toolCalls: [tool(2, "toolu_1"), tool(3, "toolu_2")],
          // seq 4 would place this last-but-one; the link to toolu_1 wins.
          pendingPermissions: [perm(4, "perm-1", "toolu_1")],
        }),
      ),
    ).toEqual(["text:before", "tool:toolu_1", "perm:perm-1", "tool:toolu_2", "text:after"]);
  });

  /* The legacy backend -- still the default -- sends no `tool_use_id` at all. A card with no link
     must still land somewhere truthful rather than being dropped or parked at the bottom. */
  it("places an unlinked permission by its own seq, which is where it arrived", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before"), msg(4, "after")],
          toolCalls: [tool(2, "toolu_1")],
          pendingPermissions: [perm(3, "perm-1", null)],
        }),
      ),
    ).toEqual(["text:before", "tool:toolu_1", "perm:perm-1", "text:after"]);
  });

  /* proto3 has no absent-string, so an unset `tool_use_id` arrives as "" on BOTH a permission and a
     tool call. Treating "" as a link would anchor an arbitrary card to an arbitrary call.
     `MessageList`'s awaiting-permission marker guards the identical case on the identical grounds. */
  it("does not treat a proto3 empty-string id as a link", () => {
    expect(
      labels(
        state({
          toolCalls: [tool(1, "")],
          pendingPermissions: [perm(2, "perm-1", "")],
        }),
      ),
    ).toEqual(["tool:", "perm:perm-1"]);
  });

  /* A card whose link names a call this state does not have -- a tool call lost to a resync, say --
     must not vanish. It falls back to its own seq like any unlinked card. */
  it("falls back to seq order when the linked tool call is not present", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before")],
          pendingPermissions: [perm(2, "perm-1", "toolu_missing")],
        }),
      ),
    ).toEqual(["text:before", "perm:perm-1"]);
  });

  it("keeps several permissions on one tool call in their own requested order", () => {
    expect(
      labels(
        state({
          toolCalls: [tool(1, "toolu_1")],
          pendingPermissions: [perm(3, "perm-b", "toolu_1"), perm(2, "perm-a", "toolu_1")],
        }),
      ),
    ).toEqual(["tool:toolu_1", "perm:perm-a", "perm:perm-b"]);
  });

  it("gives every item a key that is stable and unique across the whole timeline", () => {
    const s = state({
      transcript: [msg(1, "a"), msg(3, "b")],
      toolCalls: [tool(2, "toolu_1")],
      pendingPermissions: [perm(4, "perm-1", null)],
    });
    const keys = buildTimeline(s).map((item) => item.key);
    expect(new Set(keys).size).toBe(keys.length);
    // Stable: the same state builds the same keys, so React does not remount a streaming message.
    expect(buildTimeline(s).map((item) => item.key)).toEqual(keys);
  });

  /* The key is `seq`, not `permissionId`, and this is the case that forces the difference. proto3
     has no absent-string, so an unset `permission_id` arrives as "" -- and unlike Rust, whose
     `pending_permissions` HashMap collapses two "" ids into a single entry, `reducer.ts` appends to
     a plain array with no dedupe, so two of them genuinely coexist in frontend state. Keyed on
     `permissionId` both cards would be `key="p-"`: a React duplicate-key error and a
     mis-reconciled DOM, which is the exact failure the type's own doc claims to have eliminated. */
  it("gives two permissions sharing a proto3 empty-string id distinct keys", () => {
    const keys = buildTimeline(
      state({ pendingPermissions: [perm(1, "", null), perm(2, "", null)] }),
    ).map((item) => item.key);
    expect(keys).toEqual(["p-1", "p-2"]);
    expect(new Set(keys).size).toBe(2);
  });

  /* Two tool calls carrying ONE id -- which should not happen, and which this function must not
     amplify into a duplicate-key crash if it does. The card is emitted once, after the first call
     that claims it. Without the `anchored.delete` consumption guard both calls would emit it and
     the two copies would share a key. */
  it("emits a card once even if two tool calls claim the same id", () => {
    const items = buildTimeline(
      state({
        toolCalls: [tool(1, "toolu_dup"), tool(2, "toolu_dup")],
        pendingPermissions: [perm(3, "perm-1", "toolu_dup")],
      }),
    );
    expect(items.map((i) => i.kind)).toEqual(["tool", "permission", "tool"]);
    const keys = items.map((i) => i.key);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("is empty for a conversation that has produced nothing", () => {
    expect(buildTimeline(initialState())).toEqual([]);
  });

  /* The fourth source (Task 2 of panel-as-document): a user prompt is its own timeline item,
     ordered by `seq` exactly like the other three -- this is the merge's only new input, not a
     new merge rule.
     A second turn is included, and its prompt's `seq` (4) is neither the smallest nor the largest
     value in the fixture -- it must land BETWEEN the first tool call (3) and the second message
     (5). A fixture whose only prompt sits at the smallest `seq` cannot tell a real sort from a
     regression that special-cases prompts to always come first: `buildTimeline` spreads
     `userPrompts` into `base` before the other three collections, so "prompt first, by insertion
     order" and "prompt first, by seq" would coincide there. Landing this prompt in the middle
     requires the sort to actually run. */
  it("places a user prompt among the other three kinds by seq", () => {
    const items = buildTimeline(
      state({
        userPrompts: [{ seq: 1, text: "do the thing" }, { seq: 4, text: "and this too" }],
        transcript: [msg(2, "on it"), msg(5, "sure")],
        toolCalls: [tool(3, "toolu_1"), tool(6, "toolu_2")],
      }),
    );
    expect(items.map((i) => [i.kind, i.seq])).toEqual([
      ["prompt", 1],
      ["message", 2],
      ["tool", 3],
      ["prompt", 4],
      ["message", 5],
      ["tool", 6],
    ]);
    expect(new Set(items.map((i) => i.key)).size).toBe(6);
  });
});

describe("oldestPendingPermission (focus_permission's target)", () => {
  it("is the card with the lowest seq, wherever it is drawn", () => {
    // perm-late (seq 5) hangs under tool t1 (seq 1); perm-early (seq 3) has no call and sits at seq 3.
    const s = state({ toolCalls: [tool(1, "t1")], pendingPermissions: [perm(5, "perm-late", "t1"), perm(3, "perm-early", null)] });
    const timeline = buildTimeline(s);
    const index = oldestPendingPermission(timeline);
    expect(index).not.toBeNull();
    const item = timeline[index!];
    expect(item.kind === "permission" && item.request.permissionId).toBe("perm-early");
  });

  it("is null with no card pending", () => {
    expect(oldestPendingPermission(buildTimeline(state({ transcript: [msg(1, "hi")] })))).toBeNull();
  });
});

describe("promptIndex (R4's [[ / ]])", () => {
  it("walks to the previous or next prompt row, and stops (never wraps) at either end", () => {
    const timeline = buildTimeline(state({ userPrompts: [{ seq: 1, text: "first" }, { seq: 5, text: "second" }], transcript: [msg(2, "a"), msg(3, "b"), msg(4, "c"), msg(6, "d")] }));
    // [prompt(1), message(2), message(3), message(4), prompt(5), message(6)]
    expect(promptIndex(timeline, 3, 1)).toBe(4);
    expect(promptIndex(timeline, 4, 1)).toBeNull();
    expect(promptIndex(timeline, 3, -1)).toBe(0);
    expect(promptIndex(timeline, 0, -1)).toBeNull();
  });
});

/* v1 picks, Task 7 (R7): `]p` / `[p`, the next / previous card waiting for an answer in this tab. Wraps as
   nvim's `]d` does (`vim.diagnostic.jump`'s default) where `[[`/`]]` (`promptIndex` above) never do; a card
   this panel has already answered is not waiting, though it stays on screen until the provider resolves it. */
describe("waitingCardIndex (R7's ]p / [p)", () => {
  /** prompt 0, card p1 1, a message 2, card p2 3. */
  const fixture = () =>
    buildTimeline(
      state({
        userPrompts: [{ seq: 1, text: "go" }],
        pendingPermissions: [perm(2, "p1", null), perm(4, "p2", null)],
        transcript: [msg(3, "meanwhile")],
      }),
    );
  const none: ReadonlySet<string> = new Set();

  it("goes from the top to the first card, on to the second, and wraps from the last back to the first", () => {
    const timeline = fixture();
    expect(timeline.map((i) => i.kind)).toEqual(["prompt", "permission", "message", "permission"]);
    expect(waitingCardIndex(timeline, 0, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 1, 1, none)).toBe(3);
    expect(waitingCardIndex(timeline, 3, 1, none)).toBe(1);
  });

  it("goes back the same way: -1 from the first card wraps to the last, and from the top reaches the last too", () => {
    const timeline = fixture();
    expect(waitingCardIndex(timeline, 1, -1, none)).toBe(3);
    expect(waitingCardIndex(timeline, 3, -1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 0, -1, none)).toBe(3);
  });

  it("starts from any row, a message between two cards included", () => {
    const timeline = fixture();
    expect(waitingCardIndex(timeline, 2, 1, none)).toBe(3);
    expect(waitingCardIndex(timeline, 2, -1, none)).toBe(1);
  });

  it("skips a card this panel already answered", () => {
    const timeline = fixture();
    expect(waitingCardIndex(timeline, 0, 1, new Set(["p1"]))).toBe(3);
    expect(waitingCardIndex(timeline, 0, -1, new Set(["p2"]))).toBe(1);
    // From the answered card itself, the way on is the other one.
    expect(waitingCardIndex(timeline, 1, 1, new Set(["p1"]))).toBe(3);
    // The only waiting card is the one the search started from: it is its own next (nvim's `]d` on the
    // lone diagnostic), rather than nothing.
    expect(waitingCardIndex(timeline, 3, -1, new Set(["p1"]))).toBe(3);
  });

  it("is null when no card waits: none at all, all answered, or nothing in the timeline", () => {
    expect(waitingCardIndex(buildTimeline(state({ transcript: [msg(1, "hi")] })), 0, 1, none)).toBeNull();
    expect(waitingCardIndex(fixture(), 0, 1, new Set(["p1", "p2"]))).toBeNull();
    expect(waitingCardIndex(fixture(), 2, -1, new Set(["p1", "p2"]))).toBeNull();
    expect(waitingCardIndex([], 0, 1, none)).toBeNull();
    expect(waitingCardIndex([], 0, -1, none)).toBeNull();
  });

  it("returns a lone card from itself, both ways", () => {
    const timeline = buildTimeline(state({ userPrompts: [{ seq: 1, text: "go" }], pendingPermissions: [perm(2, "only", null)] }));
    expect(waitingCardIndex(timeline, 1, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 1, -1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 0, 1, none)).toBe(1);
  });

  it("reads a display timeline too: a collapsed run of finished calls is not a card", () => {
    const run: TimelineItem = { kind: "run", seq: 5, key: "r-5", calls: [] };
    const timeline = [...fixture(), run];
    expect(waitingCardIndex(timeline, 3, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 4, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 0, -1, none)).toBe(3);
  });

  it("is only ever a permission item: a tool call carrying a card's own id is not one", () => {
    const timeline = buildTimeline(
      state({
        toolCalls: [tool(1, "toolu_1")],
        pendingPermissions: [perm(2, "perm-1", "toolu_1")],
      }),
    );
    // tool 0, card 1: the search never stops on the call the card gates.
    expect(waitingCardIndex(timeline, 1, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 0, 1, none)).toBe(1);
    expect(waitingCardIndex(timeline, 0, -1, none)).toBe(1);
  });

  /* The cursor is an index into the timeline it was set on; one row further than the timeline now has (a
     row folded away) or a negative one must still come out as a real index, counted round the ring. */
  it("counts a from outside the timeline round it, and never answers with an index that is not one", () => {
    const timeline = fixture();
    expect(waitingCardIndex(timeline, 9, 1, none)).toBe(3); // 9 is 1 mod 4: the card after p1
    expect(waitingCardIndex(timeline, -4, -1, none)).toBe(3); // -4 is 0 mod 4: the card before the top
  });
});

/* `{N}]p` / `{N}[p`: a count repeats the jump. It is the single step taken N times, but costs one lap of the
   waiting cards at most, however large N is (the panel caps a count at 9999). */
describe("waitingCardAfter (a count before ]p / [p)", () => {
  /** prompt 0, card p1 1, a message 2, card p2 3. */
  const fixture = () =>
    buildTimeline(
      state({
        userPrompts: [{ seq: 1, text: "go" }],
        pendingPermissions: [perm(2, "p1", null), perm(4, "p2", null)],
        transcript: [msg(3, "meanwhile")],
      }),
    );
  const none: ReadonlySet<string> = new Set();

  it("is the single step for a count of one, and goes that many cards on for a larger one", () => {
    const timeline = fixture();
    expect(waitingCardAfter(timeline, 0, 1, 1, none)).toBe(waitingCardIndex(timeline, 0, 1, none));
    expect(waitingCardAfter(timeline, 0, 1, 2, none)).toBe(3);
    expect(waitingCardAfter(timeline, 0, 1, 3, none)).toBe(1); // round the ring
    expect(waitingCardAfter(timeline, 0, -1, 2, none)).toBe(1);
    expect(waitingCardAfter(timeline, 0, -1, 3, none)).toBe(3);
  });

  it("counts only the cards that wait: with p1 answered, every count lands on p2", () => {
    const timeline = fixture();
    for (const times of [1, 2, 3, 4, 9999]) expect(waitingCardAfter(timeline, 0, 1, times, new Set(["p1"])), `${times}`).toBe(3);
  });

  it("is null when no card waits, whatever the count", () => {
    expect(waitingCardAfter(fixture(), 0, 1, 5, new Set(["p1", "p2"]))).toBeNull();
    expect(waitingCardAfter([], 0, 1, 1, none)).toBeNull();
    expect(waitingCardAfter(buildTimeline(state({ transcript: [msg(1, "hi")] })), 0, -1, 3, none)).toBeNull();
  });

  it("treats a count below one as one", () => {
    const timeline = fixture();
    expect(waitingCardAfter(timeline, 0, 1, 0, none)).toBe(1);
    expect(waitingCardAfter(timeline, 0, 1, -3, none)).toBe(1);
  });

  it("is a lone card's own answer at every count", () => {
    const timeline = buildTimeline(state({ userPrompts: [{ seq: 1, text: "go" }], pendingPermissions: [perm(2, "only", null)] }));
    for (const times of [1, 2, 7, 9999]) {
      expect(waitingCardAfter(timeline, 0, 1, times, none), `${times}`).toBe(1);
      expect(waitingCardAfter(timeline, 1, -1, times, none), `${times}`).toBe(1);
    }
  });

  /* The shortcut is only allowed to be a shortcut: against the plain loop it replaces, over many small
     timelines, every start, both ways, counts past several laps, and answered subsets. */
  it("lands where repeating the single step count times does, over a spread of timelines", () => {
    let seed = 20260929;
    const next = (n: number) => {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      return seed % n;
    };
    for (let round = 0; round < 400; round++) {
      const rows = next(12);
      const cards: { seq: number; id: string }[] = [];
      const st = { userPrompts: [] as { seq: number; text: string }[], transcript: [] as TranscriptMessage[], perms: [] as PermissionRequestRecord[] };
      for (let seq = 1; seq <= rows; seq++) {
        const kind = next(3);
        if (kind === 0) st.userPrompts.push({ seq, text: `u${seq}` });
        else if (kind === 1) st.transcript.push(msg(seq, `m${seq}`));
        else {
          cards.push({ seq, id: `p${seq}` });
          st.perms.push(perm(seq, `p${seq}`, null));
        }
      }
      const timeline = buildTimeline(state({ userPrompts: st.userPrompts, transcript: st.transcript, pendingPermissions: st.perms }));
      const answered = new Set(cards.filter(() => next(3) === 0).map((c) => c.id));
      const from = next(rows + 6) - 3;
      const times = 1 + next(40);
      const delta = next(2) === 0 ? 1 : -1;
      let expected: number | null = from;
      for (let n = 0; n < times && expected !== null; n++) expected = waitingCardIndex(timeline, expected, delta, answered);
      expect(waitingCardAfter(timeline, from, delta, times, answered), `round ${round}: from ${from}, ${delta > 0 ? "+" : "-"}${times}`).toBe(expected);
    }
  });

  /* The cost bound, measured where it can be: how many cards the search looked at. The plain loop asks
     `answered` about at least one card per step; this asks about a handful, however large the count. */
  it("looks at a handful of cards for a count of 9999, not thousands", () => {
    class Counting extends Set<string> {
      asked = 0;
      has(id: string) {
        this.asked++;
        return super.has(id);
      }
    }
    const transcript = Array.from({ length: 600 }, (_, i) => msg(i + 2, `row ${i}`));
    const timeline = buildTimeline(state({ transcript, pendingPermissions: [perm(1, "only", null), perm(500, "other", null)] }));
    for (const delta of [1, -1] as const) {
      const answered = new Counting();
      const landed = waitingCardAfter(timeline, 300, delta, 9999, answered);
      expect(landed).not.toBeNull();
      expect(answered.asked, `delta ${delta}`).toBeLessThan(20);
    }
    // And still the lap the count comes to: two cards, an odd count, forwards from between them.
    const cardRows = timeline.flatMap((item, i) => (item.kind === "permission" ? [i] : []));
    expect(cardRows).toHaveLength(2);
    expect(waitingCardAfter(timeline, cardRows[0] + 1, 1, 9999, new Set())).toBe(cardRows[1]);
    expect(waitingCardAfter(timeline, cardRows[0] + 1, 1, 9998, new Set())).toBe(cardRows[0]);
  });
});
