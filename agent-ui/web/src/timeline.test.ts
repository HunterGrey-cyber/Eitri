import { describe, expect, it } from "vitest";
import { allCards, buildTimeline, oldestPendingPermission, oldestWaitingPermission, promptIndex, questionLines, waitingCardAfter, waitingCardIndex } from "./timeline";
import type { TimelineItem } from "./timeline";
import { primaryText } from "./copyText";
import { editorCardRecord, initialState } from "./reducer";
import type { AgentUiState, EditorCardWire, PermissionRequestRecord, ToolCallRecord, TranscriptMessage, TurnEnding } from "./types";

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
      case "ending":
        return `ending:${item.ending.kind}`;
      case "wake":
        return `wake:${item.text}`;
      case "notice":
        return `notice:${item.notice.kind}`;
      case "thinking":
        return `thinking:${item.block.text}${item.open ? "…" : ""}`;
      case "run":
        // `buildTimeline` itself never produces this kind -- only `display.ts`'s `buildDisplay`
        // does, over `buildTimeline`'s own output, but `TimelineItem`'s union includes it,
        // so this switch has to name it too.
        throw new Error("buildTimeline never produces a run item");
    }
  });
}

describe("buildTimeline", () => {
  it("places a turn ending where its turn stopped, between the reply and the next prompt", () => {
    const ending = { seq: 4, turnId: "t1", kind: "failed" as const, reason: null, apiErrorStatus: null, message: null };
    const items = buildTimeline(
      state({
        userPrompts: [
          { seq: 1, text: "first" },
          { seq: 5, text: "second" },
        ],
        transcript: [msg(3, "partial")],
        turnEndings: [ending],
      }),
    );
    expect(items.map((i) => i.key)).toEqual(["u-1", "m-3", "e-4", "u-5"]);
    expect(labels(state({ transcript: [msg(3, "partial")], turnEndings: [ending] }))).toEqual(["text:partial", "ending:failed"]);
  });

  /* Rendering collections separately would turn text/tool/text/tool into text/text/tool/tool,
     misrepresenting the order of the conversation. */
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

  /* The fourth source: a user prompt is its own timeline item,
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

describe("buildTimeline's item identity", () => {
  /* Rows are memoized on their item: an item must be the same object while its record is, and a new
     one when the reducer replaces the record, or a row would either draw again for nothing or keep
     showing what the record said before. */
  it("hands back the same item for the same record, and a new item for a replaced one", () => {
    const prompt = { seq: 1, text: "go" };
    const reply = msg(2, "a");
    const call = tool(3, "tu");
    const card = perm(4, "p", null);
    const ending: TurnEnding = { seq: 5, turnId: "t", kind: "interrupted", reason: null, apiErrorStatus: null, message: null };
    const done = { seq: 6, turnId: "t", text: "thought" };
    const open = { seq: 7, turnId: "t", text: "still" };
    const s = state({
      userPrompts: [prompt],
      transcript: [reply],
      toolCalls: [call],
      pendingPermissions: [card],
      turnEndings: [ending],
      thinking: [done, open],
      thinkingOpen: true,
    });
    const before = buildTimeline(s);
    const again = buildTimeline({ ...s });
    expect(again.length).toBe(before.length);
    again.forEach((item, i) => expect(item, item.key).toBe(before[i]));

    // The reducer's way of changing a record: a new object in its place. Only that item changes.
    const reply2 = { ...reply, text: "ab" };
    const call2 = { ...call, result: { content: "ok", isError: false } };
    const open2 = { ...open, text: "still more" };
    const after = buildTimeline({ ...s, transcript: [reply2], toolCalls: [call2], thinking: [done, open2] });
    const byKey = (items: TimelineItem[], key: string) => items.find((item) => item.key === key)!;
    for (const key of ["u-1", "p-4", "e-5", "k-6"]) expect(byKey(after, key), key).toBe(byKey(before, key));
    for (const key of ["m-2", "t-3", "k-7"]) expect(byKey(after, key), key).not.toBe(byKey(before, key));
    expect(byKey(after, "m-2")).toMatchObject({ kind: "message", text: "ab" });
    expect(byKey(after, "t-3")).toMatchObject({ kind: "tool", call: call2 });
    expect(byKey(after, "k-7")).toMatchObject({ kind: "thinking", block: open2, open: true });

    // A thought that closes keeps its record but not its item: `open` is drawn.
    const closed = buildTimeline({ ...s, thinkingOpen: false });
    expect(byKey(closed, "k-7")).not.toBe(byKey(before, "k-7"));
    expect(byKey(closed, "k-7")).toMatchObject({ kind: "thinking", open: false });
    expect(byKey(closed, "k-6")).toBe(byKey(before, "k-6"));
  });
});

describe("buildTimeline's identity for wake rows", () => {
  const cause = (over: Partial<import("./types").TurnCause>): import("./types").TurnCause => ({
    kind: "unknown", taskId: null, taskKind: null, taskStatus: null, description: null, summary: null,
    peerName: null, text: null, wakeupSource: null, scheduledForMs: null, scheduleReason: null, cronId: null, prompt: null, ...over,
  });

  it("hands back the same wake, task notice and folded item while their records are the same", () => {
    const scheduled = [1, 2, 3].map((seq) => ({ seq, turnId: `t${seq}`, origin: "cli" as const, cause: cause({ kind: "scheduled" }) }));
    const peer = { seq: 10, turnId: "p", origin: "cli" as const, cause: cause({ kind: "peer", peerName: "w", text: "a\nb" }) };
    const taskNotice = { seq: 20, kind: "task" as const, level: "notice" as const, content: "done", scalars: {}, taskId: "x", toolUseId: null, taskStatus: "completed" };
    const s = state({ turnStarts: [...scheduled, peer], notices: [taskNotice] });
    const before = buildTimeline(s);
    const byKey = (items: TimelineItem[], key: string) => items.find((item) => item.key === key)!;
    expect(byKey(before, "w-1")).toMatchObject({ count: 3 });
    const again = buildTimeline({ ...s });
    expect(again.length).toBe(before.length);
    again.forEach((item, i) => expect(item, item.key).toBe(before[i]));

    // A fourth empty tick is a new folded item; the peer and task rows keep theirs.
    const fourth = { seq: 4, turnId: "t4", origin: "cli" as const, cause: cause({ kind: "scheduled" }) };
    const longer = buildTimeline({ ...s, turnStarts: [...scheduled, fourth, peer] });
    expect(byKey(longer, "w-1")).toMatchObject({ count: 4 });
    expect(byKey(longer, "w-1")).not.toBe(byKey(before, "w-1"));
    expect(byKey(longer, "w-10")).toBe(byKey(before, "w-10"));
    expect(byKey(longer, "w-20")).toBe(byKey(before, "w-20"));
    // A replaced record is a new item.
    const copy = buildTimeline({ ...s, notices: [{ ...taskNotice }] });
    expect(byKey(copy, "w-20")).not.toBe(byKey(before, "w-20"));
    expect(byKey(copy, "w-20")).toMatchObject({ text: "done" });
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

describe("promptIndex ([[ / ]])", () => {
  it("walks to the previous or next prompt row, and stops (never wraps) at either end", () => {
    const timeline = buildTimeline(state({ userPrompts: [{ seq: 1, text: "first" }, { seq: 5, text: "second" }], transcript: [msg(2, "a"), msg(3, "b"), msg(4, "c"), msg(6, "d")] }));
    // [prompt(1), message(2), message(3), message(4), prompt(5), message(6)]
    expect(promptIndex(timeline, 3, 1)).toBe(4);
    expect(promptIndex(timeline, 4, 1)).toBeNull();
    expect(promptIndex(timeline, 3, -1)).toBe(0);
    expect(promptIndex(timeline, 0, -1)).toBeNull();
  });
});

/* `]p` / `[p`, the next / previous card waiting for an answer in this tab. Wraps as
   nvim's `]d` does (`vim.diagnostic.jump`'s default) where `[[`/`]]` (`promptIndex` above) never do; a card
   this panel has already answered is not waiting, though it stays on screen until the provider resolves it. */
describe("waitingCardIndex (]p / [p)", () => {
  /** Prompt 0, card p1 1, a message 2, card p2 3. */
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
  /** Prompt 0, card p1 1, a message 2, card p2 3. */
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

describe("CLI wake and notice rows", () => {
  const cause = (over: Partial<import("./types").TurnCause> = {}): import("./types").TurnCause => ({
    kind: "unknown", taskId: null, taskKind: null, taskStatus: null, description: null, summary: null,
    peerName: null, text: null, wakeupSource: null, scheduledForMs: null, scheduleReason: null, cronId: null, prompt: null, ...over,
  });
  const notice = (kind: import("./types").NoticeRecord["kind"], over: Partial<import("./types").NoticeRecord> = {}): import("./types").NoticeRecord => ({
    seq: 1, kind, level: "notice", content: null, scalars: {}, taskId: null, toolUseId: null, taskStatus: null, ...over,
  });
  const wake = (over: Partial<import("./types").TurnCause> = {}) => buildTimeline(state({
    turnStarts: [{ seq: 1, turnId: "cli", origin: "cli", cause: cause(over) }],
  }))[0];

  it.each([
    [{ kind: "scheduled", wakeupSource: "loop_wakeup", scheduleReason: "check tests" }, "Scheduled wake-up: check tests"],
    [{ kind: "scheduled", wakeupSource: "schedule_wakeup", prompt: "run tests\nthen report" }, "Scheduled task fired: run tests"],
    [{ kind: "scheduled" }, "Scheduled wake-up"],
    [{ kind: "peer", peerName: "builder", text: "ready\nall done" }, "› Message from @builder: ready"],
    [{ kind: "other" }, "Claude Code started this turn"],
    [{ kind: "unknown" }, "Claude Code started this turn"],
  ] as const)("words a CLI cause %j exactly", (over, text) => {
    expect(wake(over)).toMatchObject({ kind: "wake", text });
  });

  it("cuts the cron first line at 200 Unicode characters", () => {
    expect(wake({ kind: "scheduled", wakeupSource: "schedule_wakeup", prompt: "😀".repeat(201) + "\nignored" }))
      .toMatchObject({ text: "Scheduled task fired: " + "😀".repeat(200) });
  });

  it.each([
    ["shell", "completed", null, 'Background command "build" completed'],
    ["shell", "completed", "exit code 2: error", 'Background command "build" completed (exit code 2)'],
    ["shell", "failed", null, 'Background command "build" failed'],
    ["shell", "stopped", 'Background command "build" was stopped after reaching its background time limit', 'Background command "build" was stopped after reaching its background time limit'],
    ["remote_agent", "completed", null, 'Task "build" completed in background'],
    ["remote_agent", "stopped", null, 'Task "build" stopped'],
    ["remote_agent", "failed", null, 'Task "build" failed'],
    ["local_agent", "completed", null, 'Agent "build" finished'],
    ["workflow", "completed", 'Dynamic workflow "build" completed', 'Dynamic workflow "build" completed'],
    ["workflow", "failed", 'Dynamic workflow "build" failed: error', 'Dynamic workflow "build" failed: error'],
    ["workflow", "stopped", 'Dynamic workflow "build" was stopped', 'Dynamic workflow "build" was stopped'],
    ["monitor", "running", "event line", 'Monitor event: "build"'],
  ])("words task %s %s exactly", (taskKind, taskStatus, summary, text) => {
    expect(wake({ kind: "task", taskKind, taskStatus, summary, description: "build" })).toMatchObject({ text });
  });

  it("uses a task notification as the following turn's single cause row, worded by the turn's cause", () => {
    const rows = buildTimeline(state({
      notices: [notice("task", { taskId: "task1", taskStatus: "completed", content: "done" })],
      turnStarts: [{ seq: 2, turnId: "cli", origin: "cli", cause: cause({ kind: "task", taskId: "task1", taskKind: "shell", description: "build" }) }],
    }));
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ kind: "wake", key: "w-1", text: 'Background command "build" completed', body: "done" });
  });

  it("words a task notification with no turn by the CLI's own words, and keeps the exit code of a command", () => {
    expect(buildTimeline(state({ notices: [notice("task", { taskId: "t9", taskStatus: "completed", content: "Task t9 is done" })] }))[0])
      .toMatchObject({ text: "Task t9 is done", body: null });
    const rows = buildTimeline(state({
      notices: [notice("task", { taskId: "t1", taskStatus: "completed", content: "exit code 2: error" })],
      turnStarts: [{ seq: 2, turnId: "cli", origin: "cli", cause: cause({ kind: "task", taskId: "t1", taskKind: "shell", description: "build" }) }],
    }));
    expect(rows[0]).toMatchObject({ text: 'Background command "build" completed (exit code 2)', body: "exit code 2: error" });
  });

  it("shows a Monitor event's lines inline under its headline", () => {
    const rows = buildTimeline(state({
      notices: [notice("task", { taskId: "m1", taskStatus: "running", content: "line one\nline two" })],
      turnStarts: [{ seq: 2, turnId: "cli", origin: "cli", cause: cause({ kind: "task", taskId: "m1", taskKind: "monitor", taskStatus: "running", description: "watch" }) }],
    }));
    expect(rows[0]).toMatchObject({ text: 'Monitor event: "watch"', body: "line one\nline two", inlineBody: true });
  });

  it("gives a body only when it says more than the headline", () => {
    expect(wake({ kind: "peer", peerName: "w", text: "ready" })).toMatchObject({ text: "› Message from @w: ready", body: null });
    expect(wake({ kind: "peer", peerName: "w", text: "ready\nmore" })).toMatchObject({ body: "ready\nmore" });
  });

  it("folds three empty finished scheduled turns into the first row", () => {
    const turnStarts = [1, 2, 3].map((seq) => ({ seq, turnId: `t${seq}`, origin: "cli" as const, cause: cause({ kind: "scheduled" }) }));
    const rows = buildTimeline(state({ turnStarts }));
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ key: "w-1", count: 3 });
    expect(primaryText(rows[0])).toBe("Scheduled wake-up ×3");
    // Replies and an unfinished turn each end the run of empty ticks.
    expect(buildTimeline(state({ turnStarts, transcript: [msg(1.5, "reply")] }))).toHaveLength(3);
    expect(buildTimeline(state({ turnStarts, activeTurnId: "t3" }))).toHaveLength(2);
    // A reply after a tick belongs to that tick, so it cannot join an earlier empty tick.
    expect(buildTimeline(state({ turnStarts, transcript: [msg(2.5, "reply")] }))).toHaveLength(4);
  });

  it.each([
    ["model_fallback", "warning", "Switched model", {}, "Switched model"],
    ["model_refusal", "warning", "Cannot answer", {}, "Cannot answer"],
    ["informational", "notice", "Hook message", {}, "Hook message"],
    ["informational", "suggestion", "Try again", {}, "Try again"],
    ["informational", "warning", "Warning", {}, "Warning"],
    ["peer_hold", "warning", null, { peer_name: "worker", hold_state: "held" }, "Message from @worker held: its sender runs in a different permission mode"],
    ["peer_hold", "warning", null, { peer_name: "worker", hold_state: "dropped" }, "Message from @worker dropped after 5 minutes unanswered"],
    ["rate_limit", "notice", null, { rate_status: "reset" }, "Usage limit reset · continuing automatically"],
    ["compaction", "notice", null, { pre_tokens: 182000, post_tokens: 41000, trigger: "auto" }, "──── Conversation compacted · 182k → 41k tokens (auto) ────"],
    ["compaction", "notice", null, {}, "──── Conversation compacted ────"],
    ["hook_block", "warning", "blocked", {}, "UserPromptSubmit operation blocked by hook: blocked"],
    ["hook_block", "warning", "UserPromptSubmit operation blocked by hook: blocked", {}, "UserPromptSubmit operation blocked by hook: blocked"],
  ] as const)("words notice %s %s exactly", (kind, level, content, scalars, text) => {
    const rows = buildTimeline(state({ notices: [notice(kind, { level, content, scalars })] }));
    expect(rows).toHaveLength(1);
    expect(primaryText(rows[0])).toBe(text);
  });

  it("does not draw transient, info, released or ordinary rate notices", () => {
    const notices = [notice("api_retry"), notice("compacting"), notice("notification"),
      notice("informational", { level: "info" }), notice("peer_hold", { scalars: { hold_state: "released" } }),
      ...["allowed", "allowed_warning", "rejected"].map((rate_status) => notice("rate_limit", { scalars: { rate_status } }))];
    expect(buildTimeline(state({ notices }))).toEqual([]);
  });
});

it("marks every question of a set with its own ?, the first as the rest, since the list draws no sign cell", () => {
  const spec = (header: string, question: string) => ({ header, question, multiSelect: false, options: [{ label: "A", description: "", preview: null }] });
  const lines = questionLines({ seq: 1, interactionId: "q", toolUseId: null, turnId: null, unsupportedReason: null,
    state: { kind: "waiting" }, questions: [spec("Mode", "Choose a mode"), spec("Checks", "Choose checks")] });
  expect(lines.filter((line) => !line.muted).map((line) => line.text)).toEqual(["? Mode: Choose a mode", "? Checks: Choose checks"]);
});

/* A card Eitri's own tool broker raised sits where a provider's would: under the call it gates, or on a row of its own
   at the revision it was raised. Its key is its own id, because a later event may take the same number. */
describe("a broker card in the timeline", () => {
  function brokerCard(seq: number, n: number, toolUseId: string | null): PermissionRequestRecord {
    const wire: EditorCardWire = {
      id: `ec-0123456789abcdef-${n}`, seq, tool: "open_file", toolUseId, title: "Load a.rs in your editor?",
      body: [{ kind: "plain", text: "It is not open there." }],
    };
    return editorCardRecord(wire);
  }

  it("an_editor_card_sits_under_its_call", () => {
    const s = state({
      transcript: [msg(1, "before"), msg(5, "after")],
      toolCalls: [tool(2, "toolu_1"), tool(4, "toolu_2")],
      editorCards: [brokerCard(9, 1, "toolu_1")],
    });
    expect(labels(s)).toEqual(["text:before", "tool:toolu_1", "perm:ec-0123456789abcdef-1", "tool:toolu_2", "text:after"]);
    expect(buildTimeline(s)[2].key).toBe("ec-ec-0123456789abcdef-1");
  });

  it("one_with_no_call_gets_a_row", () => {
    // Its `seq` is the revision the next event would take, so a reply folded next shares it: both rows are there,
    // and the keys tell them apart.
    const s = state({
      transcript: [msg(1, "before"), msg(6, "next event")],
      toolCalls: [tool(2, "toolu_1")],
      editorCards: [brokerCard(6, 1, null), brokerCard(4, 2, "toolu_gone")],
    });
    const items = buildTimeline(s);
    expect(items.map((i) => i.key)).toEqual(["m-1", "t-2", "ec-ec-0123456789abcdef-2", "m-6", "ec-ec-0123456789abcdef-1"]);
    expect(labels(s)).toEqual([
      "text:before", "tool:toolu_1", "perm:ec-0123456789abcdef-2", "text:next event", "perm:ec-0123456789abcdef-1",
    ]);
    expect(new Set(items.map((i) => i.key)).size).toBe(items.length);
  });

  it("is_one_card_among_the_pending_ones_for_the_landing_and_never_for_ctrl_y", () => {
    const provider = perm(8, "perm-1", null);
    const s = state({ pendingPermissions: [provider], editorCards: [brokerCard(3, 1, null)] });
    expect(allCards(s).map((c) => c.permissionId)).toEqual(["perm-1", "ec-0123456789abcdef-1"]);
    const timeline = buildTimeline(s);
    // The landing takes the lowest `seq` of any card; `Ctrl+y` skips the broker card for the provider's.
    expect(oldestPendingPermission(timeline)).toBe(0);
    expect(oldestWaitingPermission(timeline, new Set())).toBe(1);
    expect(oldestWaitingPermission(buildTimeline(state({ editorCards: [brokerCard(3, 1, null)] })), new Set())).toBeNull();
  });
});

describe("a shell line's output", () => {
  const notice = { seq: 2, kind: "shell_output" as const, level: "notice" as const, content: "MARK", scalars: {}, task_id: null, tool_use_id: null, task_status: null };

  it("is a row of the conversation", () => {
    const items = buildTimeline(state({ notices: [notice as never] }));
    expect(items.map((item) => primaryText(item))).toContain("MARK");
  });
});

