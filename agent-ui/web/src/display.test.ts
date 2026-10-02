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

describe("P2 runs", () => {
  it("collapses two or more finished calls in a row into one summary row", () => {
    const items = [msg(1), tool(call(2, "Read")), tool(call(3, "Read")), tool(call(4, "Bash")), msg(5)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["message", "run", "message"]);
    const run = shown[1] as Extract<TimelineItem, { kind: "run" }>;
    expect(run.key).toBe("r-2");
    expect(runSummary(run.calls)).toBe("Read ×2 · Bash ×1");
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

  it("sw-panel-render-5: never folds a failing call behind a run's ✓ sign", () => {
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

  it("v1 trial item 7: an edit between reads is never counted into a fold, on either side", () => {
    // 4A (the acceptEdits fast path) auto-allows in-project Write/Edit/NotebookEdit with no card,
    // so nothing else marks an edit as different from an ordinary finished, ungated call -- without
    // this rule it would have folded into "Read ×4 · Edit ×1" and the change would have vanished
    // behind a count. A run of other tools now stops at it instead, on both sides.
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(call(3, "Edit")), tool(call(4, "Read")), tool(call(5, "Read"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["run", "tool", "run"]);
    const [before, edit, after] = shown as [Extract<TimelineItem, { kind: "run" }>, Extract<TimelineItem, { kind: "tool" }>, Extract<TimelineItem, { kind: "run" }>];
    expect(runSummary(before.calls)).toBe("Read ×2");
    expect(edit.call.toolUseId).toBe("t3");
    expect(runSummary(after.calls)).toBe("Read ×2");
  });

  it("v1 trial item 7: Write, Edit and NotebookEdit never fold into a count-only run", () => {
    const items = [tool(call(1, "Edit")), tool(call(2, "Edit")), tool(call(3, "Edit")), tool(call(4, "Edit")), tool(call(5, "Edit"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["tool", "tool", "tool", "tool", "tool"]);
    expect(shown.map((i) => (i as Extract<TimelineItem, { kind: "tool" }>).call.toolUseId)).toEqual(["t1", "t2", "t3", "t4", "t5"]);
  });

  it("v1 trial item 7: Write and NotebookEdit stop a fold the same way Edit does", () => {
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

/* `zc` (v1 picks, Task 4): closing the fold a `t-<seq>` row was unfolded from needs the run's own key,
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
