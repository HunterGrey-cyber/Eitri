import { describe, expect, it } from "vitest";
import { buildDisplay, indexOfKey, runKeyOf, runSummary } from "./display";
import type { TimelineItem } from "./timeline";
import type { ToolCallRecord } from "./types";

const call = (seq: number, name: string, done = true): ToolCallRecord => ({
  seq, toolUseId: `t${seq}`, name, input: {}, result: done ? { content: "ok", isError: false } : null,
});
const errorCall = (seq: number, name: string): ToolCallRecord => ({
  seq, toolUseId: `t${seq}`, name, input: {}, result: { content: "boom", isError: true },
});
const tool = (c: ToolCallRecord): TimelineItem => ({ kind: "tool", seq: c.seq, key: `t-${c.seq}`, call: c });
const msg = (seq: number): TimelineItem => ({ kind: "message", seq, key: `m-${seq}`, text: "m" });
const opts = { expanded: {}, detailed: false, turnRunning: false };

describe("run summaries", () => {
  it("a_run_of_an_unphrased_tool_keeps_its_name_as_written", () => {
    expect(runSummary([call(1, "acp:read"), call(2, "acp:read"), call(3, "acp:read")])).toBe("acp:read ×3");
    expect(runSummary([call(1, "Read"), call(2, "Read"), call(3, "acp:read")])).toBe("Read 2 files, acp:read ×1");
  });

  it("a_run_that_starts_with_a_phrase_is_capitalised", () => {
    expect(runSummary([call(1, "Read"), call(2, "Bash")])).toBe("Read 1 file, ran 1 command");
  });

  it("collapses two or more finished calls in a row into one summary row", () => {
    const items = [msg(1), tool(call(2, "Read")), tool(call(3, "Read")), tool(call(4, "Bash")), msg(5)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["message", "run", "message"]);
    const run = shown[1] as Extract<TimelineItem, { kind: "run" }>;
    expect(run.key).toBe("r-2");
    expect(runSummary(run.calls)).toBe("Read 2 files, ran 1 command");
  });

  it("leaves a single call, a running call, the running turn's tail and an expanded run alone", () => {
    expect(buildDisplay([msg(1), tool(call(2, "Read")), msg(3)], opts).map((i) => i.kind)).toEqual(["message", "tool", "message"]);
    expect(buildDisplay([tool(call(1, "Read")), tool(call(2, "Bash", false))], opts).map((i) => i.kind)).toEqual(["tool", "tool"]);
    expect(
      buildDisplay([tool(call(1, "Read")), tool(call(2, "Read"))], { ...opts, turnRunning: true }).map((i) => i.kind),
      "the tail of a running turn is still arriving",
    ).toEqual(["tool", "tool"]);
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), msg(3)];
    expect(buildDisplay(items, { ...opts, expanded: { "r-1": true } }).map((i) => i.kind)).toEqual(["tool", "tool", "message"]);
    expect(buildDisplay(items, { ...opts, detailed: true }).map((i) => i.kind), "Ctrl+o shows everything").toEqual(["tool", "tool", "message"]);
  });

  it("never swallows a call a card is waiting on", () => {
    const card: TimelineItem = { kind: "permission", seq: 3, key: "p-3", request: { seq: 3, permissionId: "p", toolUseId: "t2", toolName: "Bash", input: {} } };
    expect(buildDisplay([tool(call(1, "Read")), tool(call(2, "Bash")), card, msg(4)], opts).map((i) => i.kind)).toEqual(["tool", "tool", "permission", "message"]);
  });

  it("never folds a failing call behind a run's ✓ sign", () => {
    // Probe lifted from the verdict: `toolCalls [Bash isError:true, Read ok]` followed by a
    // message. Before the fix, buildDisplay's kinds were `['run', 'message']` -- the failing Bash
    // call folded into a run row that draws a fixed `✓` sign (`MessageList.tsx`) and a count-only
    // summary (`runSummary`), with no `✗` anywhere. The failing call must end the run instead, so
    // it keeps its own row -- exactly like the identical call shown alone (not folded) already does.
    const items = [tool(errorCall(1, "Bash")), tool(call(2, "Read")), msg(3)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["tool", "tool", "message"]);
  });

  it("still folds a run of all-successful calls even with a failure elsewhere in the timeline", () => {
    // Regression guard alongside the fix above: excluding a failure from the run it would otherwise
    // join must not stop two SUCCESSFUL calls next to each other from folding into a run.
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(errorCall(3, "Bash")), msg(4)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["run", "tool", "message"]);
  });

  it("an edit between reads is never counted into a fold, on either side", () => {
    // 4A (the acceptEdits fast path) auto-allows in-project Write/Edit/NotebookEdit with no card,
    // so nothing else marks an edit as different from an ordinary finished, ungated call -- without
    // this rule it would have folded into "Read 4 files, Edit ×1" and the change would have vanished
    // behind a count. A run of other tools now stops at it instead, on both sides.
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(call(3, "Edit")), tool(call(4, "Read")), tool(call(5, "Read"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["run", "tool", "run"]);
    const [before, edit, after] = shown as [Extract<TimelineItem, { kind: "run" }>, Extract<TimelineItem, { kind: "tool" }>, Extract<TimelineItem, { kind: "run" }>];
    expect(runSummary(before.calls)).toBe("Read 2 files");
    expect(edit.call.toolUseId).toBe("t3");
    expect(runSummary(after.calls)).toBe("Read 2 files");
  });

  it("Write, Edit and NotebookEdit never fold into a count-only run", () => {
    const items = [tool(call(1, "Edit")), tool(call(2, "Edit")), tool(call(3, "Edit")), tool(call(4, "Edit")), tool(call(5, "Edit"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["tool", "tool", "tool", "tool", "tool"]);
    expect(shown.map((i) => (i as Extract<TimelineItem, { kind: "tool" }>).call.toolUseId)).toEqual(["t1", "t2", "t3", "t4", "t5"]);
  });

  it("Write and NotebookEdit stop a fold the same way Edit does", () => {
    const items = [tool(call(1, "Read")), tool(call(2, "Write")), tool(call(3, "Read")), tool(call(4, "NotebookEdit")), tool(call(5, "Read"))];
    expect(buildDisplay(items, opts).map((i) => i.kind)).toEqual(["tool", "tool", "tool", "tool", "tool"]);
  });

  it("finds a row by key, inside a run too", () => {
    const shown = buildDisplay([msg(1), tool(call(2, "Read")), tool(call(3, "Read")), msg(4)], opts);
    expect(indexOfKey(shown, "m-4")).toBe(2);
    expect(indexOfKey(shown, "t-3")).toBe(1);
    expect(indexOfKey(shown, "x")).toBeNull();
  });
});

/* `zc`: closing the fold a `t-<seq>` row was unfolded from needs the run's own key,
   which `buildDisplay` derives from the run's FIRST call -- so it cannot be read off the row itself. */
describe("runKeyOf (zc)", () => {
  it("a call unfolded from a run names that run; anything else names none", () => {
    const base = [tool(call(1, "Read")), tool(call(2, "Read")), tool(errorCall(3, "Bash")), msg(4)];
    expect(runKeyOf(base, opts, "t-2")).toBe("r-1");
    expect(runKeyOf(base, opts, "t-3"), "a failed call is in no run").toBeNull();
    expect(runKeyOf(base, opts, "m-4")).toBeNull();
    expect(runKeyOf(base, { ...opts, detailed: true }, "t-2"), "Ctrl+o folds nothing").toBeNull();
  });

  it("names the run whichever of its calls is asked about, and a run key is not a call", () => {
    const base = [msg(1), tool(call(2, "Read")), tool(call(3, "Bash")), tool(call(4, "Read")), msg(5)];
    for (const key of ["t-2", "t-3", "t-4"]) expect(runKeyOf(base, opts, key), key).toBe("r-2");
    expect(runKeyOf(base, opts, "r-2"), "the run row itself is what a fold closes, not a call in it").toBeNull();
    expect(runKeyOf(base, opts, "m-1")).toBeNull();
    expect(runKeyOf(base, opts, "t-99"), "a seq no run holds").toBeNull();
  });

  it("finds the run as it would fold now, even while it is unfolded (expanded is ignored)", () => {
    const base = [tool(call(1, "Read")), tool(call(2, "Read")), msg(3)];
    expect(runKeyOf(base, { ...opts, expanded: { "r-1": true } }, "t-2")).toBe("r-1");
  });

  it("a lone call and the running turn's still-arriving tail are in no run", () => {
    expect(runKeyOf([tool(call(1, "Read")), msg(2)], opts, "t-1")).toBeNull();
    const tail = [tool(call(1, "Read")), tool(call(2, "Read"))];
    expect(runKeyOf(tail, { ...opts, turnRunning: true }, "t-2")).toBeNull();
    expect(runKeyOf(tail, opts, "t-2"), "the same calls once the turn is over").toBe("r-1");
  });

  it("an edit ends a run: it is in none, and the reads either side are in their own", () => {
    const base = [tool(call(1, "Read")), tool(call(2, "Read")), tool(call(3, "Edit")), tool(call(4, "Read")), tool(call(5, "Read"))];
    expect(runKeyOf(base, opts, "t-2")).toBe("r-1");
    expect(runKeyOf(base, opts, "t-3")).toBeNull();
    expect(runKeyOf(base, opts, "t-5")).toBe("r-4");
  });
});

describe("a turn ending in a run of finished calls", () => {
  it("is never folded into the run: the run stops at it, on both sides", () => {
    const ending: TimelineItem = { kind: "ending", seq: 4, key: "e-4", ending: { seq: 4, turnId: "t", kind: "failed", reason: null, apiErrorStatus: null, message: null } };
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(call(3, "Read")), ending, tool(call(5, "Read")), tool(call(6, "Read"))];
    expect(buildDisplay(items, opts).map((i) => i.kind)).toEqual(["run", "ending", "run"]);
    expect(buildDisplay(items, { ...opts, detailed: true }).map((i) => i.kind)).toEqual(items.map((i) => i.kind));
  });
});

describe("runs and turns", () => {
  const inTurn = (seq: number, turnId: string): TimelineItem => tool({ ...call(seq, "Read"), turnId });
  const thoughtIn = (seq: number, turnId: string): TimelineItem => ({
    kind: "thinking",
    seq,
    key: `k-${seq}`,
    block: { seq, turnId, text: "t" },
    open: false,
  });

  it("a run never crosses into the next turn, so each turn's line has a row of its own to stand under", () => {
    const items = [inTurn(1, "t1"), inTurn(2, "t1"), inTurn(3, "t2"), inTurn(4, "t2")];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.key)).toEqual(["r-1", "r-3"]);
    // A thought of the next turn ends the run too, and is not folded into it.
    const withThought = [inTurn(1, "t1"), inTurn(2, "t1"), thoughtIn(3, "t2"), inTurn(4, "t2"), inTurn(5, "t2")];
    expect(buildDisplay(withThought, opts).map((i) => i.key)).toEqual(["r-1", "k-3", "r-4"]);
    // An item that names no turn joins either side.
    expect(buildDisplay([inTurn(1, "t1"), tool(call(2, "Read")), inTurn(3, "t1")], opts).map((i) => i.key)).toEqual(["r-1"]);
  });
});

describe("thinking among the rows", () => {
  const thought = (seq: number, open = false): TimelineItem => ({
    kind: "thinking",
    seq,
    key: `k-${seq}`,
    block: { seq, turnId: "t", text: `thought ${seq}` },
    open,
  });
  const kinds = (items: TimelineItem[]) => items.map((i) => i.kind);

  it("collapsed and preview fold a finished thought between two calls into their run; one before or after stays a row", () => {
    const items = [thought(1), tool(call(2, "Read")), thought(3), tool(call(4, "Read")), thought(5), msg(6)];
    for (const thinking of ["collapsed", "preview", undefined] as const) {
      const shown = buildDisplay(items, { ...opts, thinking });
      expect(kinds(shown), String(thinking)).toEqual(["thinking", "run", "thinking", "message"]);
      const run = shown[1] as Extract<TimelineItem, { kind: "run" }>;
      expect(run.calls.map((c) => c.seq)).toEqual([2, 4]);
      expect(run.thoughts?.map((t) => t.seq)).toEqual([3]);
    }
    // Opened, the run gives back every row it held, the thought among them.
    expect(kinds(buildDisplay(items, { ...opts, expanded: { "r-2": true } }))).toEqual(kinds(items));
  });

  it("a thought still being written is never folded away", () => {
    const items = [tool(call(1, "Read")), thought(2, true), tool(call(3, "Read")), msg(4)];
    expect(kinds(buildDisplay(items, opts))).toEqual(["tool", "thinking", "tool", "message"]);
  });

  it("expanded folds no thought; hidden leaves every thought out and folds as if there were none", () => {
    const items = [tool(call(1, "Read")), thought(2), tool(call(3, "Read")), msg(4)];
    expect(kinds(buildDisplay(items, { ...opts, thinking: "expanded" }))).toEqual(["tool", "thinking", "tool", "message"]);
    expect(kinds(buildDisplay(items, { ...opts, thinking: "hidden" }))).toEqual(["run", "message"]);
    expect(kinds(buildDisplay([thought(1), msg(2)], { ...opts, thinking: "hidden" }))).toEqual(["message"]);
  });

  it("the detailed view keeps every thought whatever the level", () => {
    const items = [tool(call(1, "Read")), thought(2), tool(call(3, "Read")), msg(4)];
    for (const thinking of ["collapsed", "preview", "expanded", "hidden"] as const) {
      expect(kinds(buildDisplay(items, { ...opts, thinking, detailed: true })), thinking).toEqual(kinds(items));
    }
  });

  it("a run that only thinking follows is still the running turn's tail", () => {
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), thought(3, true)];
    expect(kinds(buildDisplay(items, { ...opts, turnRunning: true }))).toEqual(["tool", "tool", "thinking"]);
    expect(kinds(buildDisplay(items, opts))).toEqual(["run", "thinking"]);
  });

  it("finds a thought folded into a run at the run, and zc from it closes that run", () => {
    const items = [tool(call(1, "Read")), thought(2), tool(call(3, "Read")), msg(4)];
    const shown = buildDisplay(items, opts);
    expect(indexOfKey(shown, "k-2")).toBe(0);
    expect(runKeyOf(items, opts, "k-2")).toBe("r-1");
    expect(runKeyOf(items, opts, "k-9")).toBeNull();
  });
});

describe("a run's identity", () => {
  /* Rows are memoized on their item, so a run that folds the same members must be the same object
     from one build to the next, and a new one as soon as any member changes. */
  it("is the same object while its members are, and a new one when a member changes", () => {
    const a = tool(call(1, "Read"));
    const b = tool(call(2, "Read"));
    const between: TimelineItem = { kind: "thinking", seq: 3, key: "k-3", block: { seq: 3, turnId: "t", text: "x" }, open: false };
    const c = tool(call(4, "Bash"));
    const m = msg(5);
    const first = buildDisplay([a, b, between, c, m], opts)[0];
    expect(first.kind).toBe("run");
    expect(buildDisplay([a, b, between, c, m], opts)[0]).toBe(first);
    expect(buildDisplay([a, b, between, c, msg(5)], opts)[0], "a row outside the run changed").toBe(first);

    const c2 = tool({ ...call(4, "Bash"), name: "Grep" });
    const changed = buildDisplay([a, b, between, c2, m], opts)[0] as Extract<TimelineItem, { kind: "run" }>;
    expect(changed).not.toBe(first);
    expect(runSummary(changed.calls)).toBe("Read 2 files, searched 1 pattern");

    const shorter = buildDisplay([a, b, m], opts)[0] as Extract<TimelineItem, { kind: "run" }>;
    expect(shorter).not.toBe(first);
    expect(shorter.calls.map((x) => x.seq)).toEqual([1, 2]);
    expect(shorter.thoughts).toBeUndefined();
  });
});

it("reuses expanded child items until their record, depth or open state changes", () => {
  const childCall = call(2, "Read");
  const block = { seq: 4, turnId: "t", text: "thinking" };
  const message = { seq: 3, text: "child reply" };
  const stream = { toolCalls: [childCall], transcript: [message], thinking: [block],
    assistantMessageOpen: false, thinkingOpen: true };
  const parent = { ...call(1, "Agent"), children: stream };
  const options = { ...opts, expanded: { "t-1": true, "t-5": true } };
  const build = (record = parent) => buildDisplay([tool(record), msg(10)], options);
  const first = build();
  const again = build({ ...parent });
  for (let i = 1; i <= 3; i++) expect(again[i]).toBe(first[i]);
  const closed = build({ ...parent, children: { ...stream, thinkingOpen: false } });
  expect(closed[1]).toBe(first[1]);
  expect(closed[2]).toBe(first[2]);
  expect(closed[3]).not.toBe(first[3]);
  expect(closed[3]).toMatchObject({ kind: "thinking", open: false, depth: 1 });
  const changed = build({ ...parent, children: { ...stream, transcript: [{ ...message, text: "changed" }] } });
  expect(changed[2]).not.toBe(first[2]);
  expect(changed[2]).toMatchObject({ text: "changed" });
  const nested = { ...call(5, "Agent"), children: stream };
  const deeper = buildDisplay([tool(nested)], { ...options, detailed: true });
  const deepest = buildDisplay([tool({ ...parent, children: { ...stream, toolCalls: [nested], transcript: [], thinking: [] } })],
    { ...options, detailed: true });
  expect(deepest[2]).not.toBe(deeper[1]);
  expect(deepest[2]).toMatchObject({ key: "t-2", depth: 2 });
});
